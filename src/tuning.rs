//! Kernel tuning with guaranteed restore.
//!
//! Every sysctl / sysfs knob Wolfram changes goes through `Tuning::set`, which
//! reads the current value and persists it to `original-tuning.json` in the
//! state directory BEFORE writing the new value. `restore()` writes the
//! originals back and deletes the file.
//!
//! The file survives a crash, a SIGKILL or a panic, so the originals are never
//! lost: on the next start the recorded values stay authoritative (we never
//! re-record a knob that is already in the file, because its live value is
//! then our own), and `wolfram --restore-tuning` (run by the systemd unit's
//! ExecStopPost and by uninstall.sh) restores from the file without starting
//! the daemon.
//!
//! The file is tagged with the kernel boot ID. A file from an earlier boot is
//! discarded: the kernel reset every knob at boot, so its "originals" are
//! stale and its zram device numbers may now belong to someone else.
//!
//! The zram devices Wolfram creates are recorded here too, so a restore after
//! a crash can remove them before re-enabling zswap.

use anyhow::{Context, Result};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const STATE_FILE: &str = "original-tuning.json";
const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";

#[derive(Serialize, Deserialize, Default)]
struct Saved {
    #[serde(default)]
    boot_id: String,
    /// (path, original value) in the order they were first changed.
    #[serde(default)]
    entries: Vec<(String, String)>,
    /// zram device indices Wolfram created and has not yet removed.
    #[serde(default)]
    zram_devices: Vec<u32>,
}

impl Saved {
    fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.zram_devices.is_empty()
    }
}

fn current_boot_id() -> String {
    fs::read_to_string(BOOT_ID).map(|s| s.trim().to_string()).unwrap_or_default()
}

pub struct Tuning {
    file: PathBuf,
    saved: Saved,
}

impl Tuning {
    /// Load any originals left by a previous run that did not restore them.
    pub fn load(state_dir: &Path) -> Self {
        Self::load_for_boot(state_dir, &current_boot_id())
    }

    fn load_for_boot(state_dir: &Path, boot_id: &str) -> Self {
        let file = state_dir.join(STATE_FILE);
        let fresh = Saved { boot_id: boot_id.to_string(), ..Saved::default() };
        let saved = match fs::read_to_string(&file) {
            Ok(s) => match serde_json::from_str::<Saved>(&s) {
                Ok(v) if v.boot_id != boot_id => {
                    info!(
                        "Discarding {} from an earlier boot (kernel values were reset by the reboot)",
                        file.display()
                    );
                    fs::remove_file(&file).ok();
                    fresh
                }
                Ok(v) => {
                    if !v.is_empty() {
                        info!(
                            "Found {} original kernel value(s) from a previous run in {} — keeping them for restore",
                            v.entries.len(),
                            file.display()
                        );
                    }
                    v
                }
                Err(e) => {
                    warn!("Ignoring unreadable {}: {e}", file.display());
                    fresh
                }
            },
            Err(_) => fresh,
        };
        Self { file, saved }
    }

    /// Set `path` to `value`, recording the original first.
    /// Returns Ok(true) if the value was changed, Ok(false) if the knob is
    /// missing on this kernel or already holds `value`.
    pub fn set(&mut self, path: &str, value: &str) -> Result<bool> {
        let p = Path::new(path);
        if !p.exists() {
            return Ok(false);
        }
        let current = fs::read_to_string(p)
            .with_context(|| format!("reading {path}"))?
            .trim()
            .to_string();
        if current == value {
            return Ok(false);
        }
        if !self.saved.entries.iter().any(|(k, _)| k == path) {
            self.saved.entries.push((path.to_string(), current));
            // Persist before changing anything, so a crash between the two
            // steps can never lose the original.
            self.persist()
                .with_context(|| format!("recording original value of {path}"))?;
        }
        fs::write(p, value).with_context(|| format!("writing {value} to {path}"))?;
        Ok(true)
    }

    fn persist(&self) -> Result<()> {
        if let Some(dir) = self.file.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = self.file.with_extension("tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&self.saved)?)?;
        fs::rename(&tmp, &self.file)?;
        Ok(())
    }

    /// Record a zram device we just created (persisted before it is used).
    pub fn record_zram(&mut self, idx: u32) {
        if !self.saved.zram_devices.contains(&idx) {
            self.saved.zram_devices.push(idx);
            if let Err(e) = self.persist() {
                warn!("Could not record zram{idx} in {}: {e}", self.file.display());
            }
        }
    }

    /// Forget a zram device we removed.
    pub fn forget_zram(&mut self, idx: u32) {
        let before = self.saved.zram_devices.len();
        self.saved.zram_devices.retain(|&d| d != idx);
        if self.saved.zram_devices.len() != before {
            if self.saved.is_empty() {
                fs::remove_file(&self.file).ok();
            } else if let Err(e) = self.persist() {
                warn!("Could not update {}: {e}", self.file.display());
            }
        }
    }

    /// Remove zram devices a previous instance (this boot) created but never
    /// removed — it was killed or crashed before its own cleanup.
    pub fn remove_leftover_zram(&mut self) {
        for idx in self.saved.zram_devices.clone() {
            let dev = format!("/dev/zram{idx}");
            info!("Removing leftover {dev} from a previous run");
            let off = std::process::Command::new("swapoff").arg(&dev).output();
            let swapped_off = matches!(&off, Ok(o) if o.status.success());
            // swapoff fails harmlessly if the device is not active swap.
            let removed = fs::write("/sys/class/zram-control/hot_remove", idx.to_string()).is_ok();
            if removed || !Path::new(&format!("/sys/block/zram{idx}")).exists() {
                self.forget_zram(idx);
            } else {
                warn!(
                    "Could not remove leftover {dev} (swapoff {}) — leaving it recorded",
                    if swapped_off { "ok" } else { "failed" }
                );
            }
        }
    }

    /// Write every recorded original back (newest change first) and delete the
    /// state file. Knobs that fail to restore stay in the file so a later
    /// `--restore-tuning` can retry them.
    pub fn restore(&mut self) {
        if self.saved.entries.is_empty() {
            if self.saved.is_empty() {
                fs::remove_file(&self.file).ok();
            }
            return;
        }
        let mut failed = Vec::new();
        for (path, original) in self.saved.entries.iter().rev() {
            match fs::write(path, original) {
                Ok(()) => info!("  restored {path} = {original}"),
                Err(e) => {
                    warn!("  could not restore {path} = {original}: {e}");
                    failed.push((path.clone(), original.clone()));
                }
            }
        }
        failed.reverse();
        self.saved.entries = failed;
        if self.saved.is_empty() {
            fs::remove_file(&self.file).ok();
        } else if let Err(e) = self.persist() {
            warn!("Could not update {}: {e}", self.file.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wolfram-tuning-{}-{name}", std::process::id()));
        fs::remove_dir_all(&d).ok();
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn set_records_then_restore_puts_back() {
        let d = scratch("basic");
        let knob = d.join("swappiness");
        fs::write(&knob, "150\n").unwrap();
        let k = knob.to_str().unwrap();

        let mut t = Tuning::load_for_boot(&d, "b");
        assert!(t.set(k, "100").unwrap());
        assert_eq!(fs::read_to_string(&knob).unwrap(), "100");
        assert!(d.join(STATE_FILE).exists(), "original persisted before change");
        // Setting again does not overwrite the recorded original.
        assert!(t.set(k, "90").unwrap());

        t.restore();
        assert_eq!(fs::read_to_string(&knob).unwrap(), "150");
        assert!(!d.join(STATE_FILE).exists());
    }

    #[test]
    fn unchanged_or_missing_knob_is_not_recorded() {
        let d = scratch("noop");
        let knob = d.join("k");
        fs::write(&knob, "0\n").unwrap();
        let mut t = Tuning::load_for_boot(&d, "b");
        assert!(!t.set(knob.to_str().unwrap(), "0").unwrap());
        assert!(!t.set(d.join("absent").to_str().unwrap(), "1").unwrap());
        assert!(!d.join(STATE_FILE).exists());
    }

    #[test]
    fn state_from_an_earlier_boot_is_discarded() {
        let d = scratch("reboot");
        let knob = d.join("swappiness");
        fs::write(&knob, "60\n").unwrap();
        let k = knob.to_str().unwrap();

        let mut before = Tuning::load_for_boot(&d, "boot-a");
        before.set(k, "100").unwrap();
        before.record_zram(3);
        drop(before); // power loss: no restore

        // After the reboot the admin's value is 30; the stale 60 must not win.
        fs::write(&knob, "30\n").unwrap();
        let mut after = Tuning::load_for_boot(&d, "boot-b");
        assert!(after.saved.zram_devices.is_empty(), "old boot's zram numbers dropped");
        after.set(k, "100").unwrap();
        after.restore();
        assert_eq!(fs::read_to_string(&knob).unwrap(), "30");
    }

    #[test]
    fn zram_record_keeps_file_until_forgotten() {
        let d = scratch("zram");
        let mut t = Tuning::load_for_boot(&d, "b");
        t.record_zram(1);
        t.restore(); // no knob entries, but a live device: file must stay
        assert!(d.join(STATE_FILE).exists());
        t.forget_zram(1);
        assert!(!d.join(STATE_FILE).exists());
    }

    #[test]
    fn crash_then_restart_keeps_true_originals() {
        let d = scratch("crash");
        let knob = d.join("vfs_cache_pressure");
        fs::write(&knob, "50\n").unwrap();
        let k = knob.to_str().unwrap();

        // First run changes the knob and "crashes" (no restore).
        let mut first = Tuning::load_for_boot(&d, "b");
        first.set(k, "200").unwrap();
        drop(first);

        // Second run sees 200 live, but must keep 50 as the original.
        let mut second = Tuning::load_for_boot(&d, "b");
        second.set(k, "200").unwrap();
        second.restore();
        assert_eq!(fs::read_to_string(&knob).unwrap(), "50");
    }
}

//! CRIU-based process hibernation — checkpoint processes to disk and restore on demand.
//!
//! Uses `criu dump` to serialize a process's complete state (memory, file descriptors,
//! sockets, registers) to a directory on disk, then kills the process.
//! `criu restore` brings it back exactly as it was.
//!
//! Requires: criu installed, root privileges.

use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;

use crate::config::Config;
use crate::scanner::ProcessInfo;

/// A hibernated process — checkpointed to disk, no longer running.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields used in Debug output and future expansion
struct HibernatedProcess {
    pid: u32,
    name: String,
    checkpoint_dir: PathBuf,
    rss_mib: u64,
    /// TCP ports the process was listening on (for port proxy).
    listening_ports: Vec<u16>,
}

pub struct HibernationStats {
    pub hibernated: usize,
    pub saved_mib: u64,
}

/// Manages hibernated processes.
pub struct Hibernator {
    checkpoint_base: PathBuf,
    enable_port_proxy: bool,
    /// PID -> hibernated process info.
    hibernated: HashMap<u32, HibernatedProcess>,
    /// Port -> (original PID, TCP listener holding the port for wake-on-connect).
    port_proxies: HashMap<u16, (u32, TcpListener)>,
    dry_run: bool,
}

impl Hibernator {
    pub fn new(config: &Config) -> Self {
        Self {
            checkpoint_base: config.checkpoint_dir.clone(),
            enable_port_proxy: config.enable_port_proxy,
            hibernated: HashMap::new(),
            port_proxies: HashMap::new(),
            dry_run: config.dry_run,
        }
    }

    /// Checkpoint a process to disk and kill it.
    pub fn checkpoint(&mut self, proc_info: &ProcessInfo) -> Result<()> {
        let pid = proc_info.pid;

        if self.hibernated.contains_key(&pid) {
            anyhow::bail!("PID {pid} is already hibernated");
        }

        // Create checkpoint directory.
        let checkpoint_dir = self.checkpoint_base.join(format!("pid_{pid}"));
        if checkpoint_dir.exists() {
            fs::remove_dir_all(&checkpoint_dir)?;
        }
        fs::create_dir_all(&checkpoint_dir)
            .with_context(|| format!("creating checkpoint dir {}", checkpoint_dir.display()))?;

        // Find listening TCP ports before we kill the process.
        let ports = find_listening_ports(pid);
        debug!("  PID {pid} listening on ports: {ports:?}");

        if self.dry_run {
            info!(
                "  [DRY RUN] Would checkpoint PID {pid} ({}) to {}",
                proc_info.name,
                checkpoint_dir.display()
            );
            fs::remove_dir_all(&checkpoint_dir).ok();
            return Ok(());
        }

        // Run criu dump.
        let output = Command::new("criu")
            .arg("dump")
            .arg("-t")
            .arg(pid.to_string())
            .arg("-D")
            .arg(&checkpoint_dir)
            .arg("--shell-job")
            .arg("--tcp-established")
            .arg("-v0")
            .output()
            .context("running criu dump — is criu installed?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            fs::remove_dir_all(&checkpoint_dir).ok();
            anyhow::bail!("criu dump failed for PID {pid}: {stderr}");
        }

        // Process is now dead — CRIU killed it after checkpointing.
        let entry = HibernatedProcess {
            pid,
            name: proc_info.name.clone(),
            checkpoint_dir: checkpoint_dir.clone(),
            rss_mib: proc_info.rss_kib / 1024,
            listening_ports: ports.clone(),
        };

        // Set up port proxies for transparent wake-on-connect.
        if self.enable_port_proxy {
            for &port in &ports {
                match TcpListener::bind(format!("0.0.0.0:{port}")) {
                    Ok(listener) => {
                        listener.set_nonblocking(true).ok();
                        info!("  Port proxy: listening on :{port} for PID {pid} wake-up");
                        self.port_proxies.insert(port, (pid, listener));
                    }
                    Err(e) => {
                        warn!("  Could not bind port {port} for proxy: {e}");
                    }
                }
            }
        }

        self.hibernated.insert(pid, entry);
        Ok(())
    }

    /// Restore a hibernated process from checkpoint.
    pub fn restore(&mut self, pid: u32) -> Result<()> {
        let entry = self
            .hibernated
            .remove(&pid)
            .with_context(|| format!("PID {pid} is not hibernated"))?;

        // Release port proxies BEFORE restoring so the ports are free.
        let mut released_proxies = Vec::new();
        for &port in &entry.listening_ports {
            if let Some(proxy) = self.port_proxies.remove(&port) {
                released_proxies.push((port, proxy));
                debug!("  Released port proxy on :{port}");
            }
        }

        info!(
            "Restoring {} (PID {pid}) from {}",
            entry.name,
            entry.checkpoint_dir.display()
        );

        if self.dry_run {
            info!("  [DRY RUN] Would restore PID {pid}");
            return Ok(());
        }

        // Drop all released proxy listeners to free the ports before CRIU restore.
        drop(released_proxies);

        let output = Command::new("criu")
            .arg("restore")
            .arg("-d")
            .arg("-D")
            .arg(&entry.checkpoint_dir)
            .arg("--shell-job")
            .arg("--tcp-established")
            .arg("-v0")
            .output()
            .context("running criu restore")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Restore failed — re-create port proxies and put the entry back.
            if self.enable_port_proxy {
                for &port in &entry.listening_ports {
                    match TcpListener::bind(format!("0.0.0.0:{port}")) {
                        Ok(listener) => {
                            listener.set_nonblocking(true).ok();
                            self.port_proxies.insert(port, (pid, listener));
                        }
                        Err(e) => {
                            warn!("  Could not re-bind port {port} after failed restore: {e}");
                        }
                    }
                }
            }
            self.hibernated.insert(pid, entry);
            anyhow::bail!("criu restore failed for PID {pid}: {stderr}");
        }

        // Clean up checkpoint files.
        fs::remove_dir_all(&entry.checkpoint_dir).ok();

        info!(
            "  Restored {} (PID {pid}) — ~{} MiB back in RAM",
            entry.name, entry.rss_mib
        );
        Ok(())
    }

    /// Restore all hibernated processes (for graceful shutdown).
    pub fn restore_all(&mut self) {
        let pids: Vec<u32> = self.hibernated.keys().copied().collect();
        for pid in pids {
            if let Err(e) = self.restore(pid) {
                error!("Failed to restore PID {pid}: {e}");
            }
        }
    }

    /// Check port proxies for incoming connections — triggers restore.
    pub fn check_port_proxies(&mut self) {
        let mut wake_pids: Vec<u32> = Vec::new();

        for (&port, (pid, listener)) in &self.port_proxies {
            match listener.accept() {
                Ok((stream, addr)) => {
                    info!(
                        "  Wake-on-connect: :{port} from {addr} — restoring PID {pid}"
                    );
                    drop(stream);
                    if !wake_pids.contains(pid) {
                        wake_pids.push(*pid);
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    warn!("  Port proxy :{port} accept error: {e}");
                }
            }
        }

        for pid in wake_pids {
            if let Err(e) = self.restore(pid) {
                error!("  Wake-on-connect restore failed for PID {pid}: {e}");
            }
        }
    }

    pub fn has_hibernated(&self) -> bool {
        !self.hibernated.is_empty()
    }

    pub fn stats(&self) -> HibernationStats {
        let saved_mib: u64 = self.hibernated.values().map(|h| h.rss_mib).sum();
        HibernationStats {
            hibernated: self.hibernated.len(),
            saved_mib,
        }
    }
}

/// Find TCP ports a process is listening on.
///
/// Cross-references /proc/[pid]/fd/ socket inodes with /proc/[pid]/net/tcp
/// to only return ports actually owned by this specific PID.
fn find_listening_ports(pid: u32) -> Vec<u16> {
    // Step 1: Collect socket inodes owned by this process.
    let fd_dir = format!("/proc/{pid}/fd");
    let mut socket_inodes: Vec<u64> = Vec::new();

    if let Ok(entries) = fs::read_dir(&fd_dir) {
        for entry in entries.flatten() {
            if let Ok(link) = fs::read_link(entry.path()) {
                let link_str = link.to_string_lossy();
                // Socket links look like "socket:[12345]"
                if let Some(rest) = link_str.strip_prefix("socket:[") {
                    if let Some(inode_str) = rest.strip_suffix(']') {
                        if let Ok(inode) = inode_str.parse::<u64>() {
                            socket_inodes.push(inode);
                        }
                    }
                }
            }
        }
    }

    if socket_inodes.is_empty() {
        return Vec::new();
    }

    // Step 2: Read /proc/[pid]/net/tcp and tcp6, find LISTEN sockets matching our inodes.
    let mut ports = Vec::new();

    for tcp_file in &["tcp", "tcp6"] {
        let path = format!("/proc/{pid}/net/{tcp_file}");
        let contents = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        for line in contents.lines().skip(1) {
            // skip header per file
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 {
                continue;
            }
            // fields[3] = state, 0A = LISTEN
            if fields[3] != "0A" {
                continue;
            }
            // fields[9] = inode
            let inode: u64 = match fields[9].parse() {
                Ok(i) => i,
                Err(_) => continue,
            };
            if !socket_inodes.contains(&inode) {
                continue; // not our socket
            }
            // fields[1] = local_address "hex_ip:hex_port"
            if let Some(port_hex) = fields[1].split(':').nth(1) {
                if let Ok(port) = u16::from_str_radix(port_hex, 16) {
                    if port > 0 && !ports.contains(&port) {
                        ports.push(port);
                    }
                }
            }
        }
    }

    ports
}

/// Check if CRIU is available.
pub fn is_supported() -> bool {
    Command::new("criu")
        .arg("check")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

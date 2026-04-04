//! Configuration — TOML file + CLI argument overrides.

use anyhow::{Context, Result};
use clap::Parser;
use serde::Deserialize;
use std::path::PathBuf;

/// Wolfram — proactive memory compression and process hibernation daemon for Linux.
#[derive(Parser, Debug)]
#[command(name = "wolfram", version, about)]
struct Cli {
    /// Path to config file (default: /etc/wolfram/config.toml)
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Minimum process anonymous memory in MiB to consider
    #[arg(long)]
    min_anon: Option<u64>,

    /// Minimum region size in KiB to track
    #[arg(long)]
    min_region: Option<u64>,

    /// Seconds to sample idle pages
    #[arg(long)]
    idle_sample: Option<u64>,

    /// Seconds between full scan cycles
    #[arg(long)]
    cycle: Option<u64>,

    /// Idle fraction (0.0-1.0) to trigger MADV_COLD
    #[arg(long)]
    cold_threshold: Option<f64>,

    /// Idle fraction (0.0-1.0) to trigger MADV_PAGEOUT
    #[arg(long)]
    pageout_threshold: Option<f64>,

    /// Consecutive idle cycles before escalating to PAGEOUT
    #[arg(long)]
    pageout_cycles: Option<u32>,

    /// Enable CRIU-based process hibernation
    #[arg(long)]
    hibernate: bool,

    /// Consecutive fully-idle cycles before hibernating a process
    #[arg(long)]
    hibernate_cycles: Option<u32>,

    /// Directory to store CRIU checkpoints
    #[arg(long)]
    checkpoint_dir: Option<PathBuf>,

    /// Enable port proxy for transparent wake-on-connect
    #[arg(long)]
    port_proxy: bool,

    /// Process names to exclude (comma-separated)
    #[arg(long, value_delimiter = ',')]
    exclude: Vec<String>,

    /// PIDs to exclude (comma-separated)
    #[arg(long, value_delimiter = ',')]
    exclude_pid: Vec<u32>,

    /// Dry run — scan and report but don't compress or hibernate
    #[arg(long)]
    dry_run: bool,

    /// Increase log verbosity (use -v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

/// File-based configuration (TOML).
#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct FileConfig {
    min_process_anon_mib: Option<u64>,
    min_region_kib: Option<u64>,
    idle_sample_secs: Option<u64>,
    cycle_secs: Option<u64>,
    cold_threshold: Option<f64>,
    pageout_threshold: Option<f64>,
    pageout_after_cycles: Option<u32>,
    enable_hibernation: Option<bool>,
    hibernate_after_cycles: Option<u32>,
    checkpoint_dir: Option<String>,
    enable_port_proxy: Option<bool>,
    excluded_names: Option<Vec<String>>,
    excluded_pids: Option<Vec<u32>>,
    dry_run: Option<bool>,
}

/// Resolved runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub min_process_anon_mib: u64,
    pub min_region_kib: u64,
    pub idle_sample_secs: u64,
    pub cycle_secs: u64,
    pub cold_threshold: f64,
    pub pageout_threshold: f64,
    pub pageout_after_cycles: u32,
    pub enable_hibernation: bool,
    pub hibernate_after_cycles: u32,
    pub checkpoint_dir: PathBuf,
    pub enable_port_proxy: bool,
    pub excluded_names: Vec<String>,
    pub excluded_pids: Vec<u32>,
    pub dry_run: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            min_process_anon_mib: 50,
            min_region_kib: 1024,
            idle_sample_secs: 30,
            cycle_secs: 60,
            cold_threshold: 0.7,
            pageout_threshold: 0.9,
            pageout_after_cycles: 3,
            enable_hibernation: false,
            hibernate_after_cycles: 10,
            checkpoint_dir: PathBuf::from("/var/lib/wolfram/checkpoints"),
            enable_port_proxy: false,
            excluded_names: vec![
                // Core system
                "systemd".into(),
                "wolfram".into(),
                "sshd".into(),
                // Container runtimes
                "dockerd".into(),
                "containerd".into(),
                "containerd-shim".into(),
                "lxc-start".into(),
                // Kubernetes
                "kubelet".into(),
                "kube-proxy".into(),
                "kube-apiserver".into(),
                "kube-controller".into(), // truncated from kube-controller-manager
                "kube-scheduler".into(),
                "etcd".into(),
                // Virtualisation
                "qemu-system".into(),
                // Proxmox
                "pvedaemon".into(),
                "pveproxy".into(),
                "pvestatd".into(),
                "pvefw".into(),
                "pmxcfs".into(),
                "corosync".into(),
                // Storage
                "ceph-osd".into(),
                "ceph-mon".into(),
            ],
            excluded_pids: vec![1],
            dry_run: false,
        }
    }
}

/// Load config and return (config, log_level_string).
/// Parses CLI args once, loads TOML file, merges with CLI overrides.
pub fn load() -> Result<(Config, String)> {
    let cli = Cli::parse();

    let log_level = match cli.verbose {
        0 => "info".into(),
        1 => "debug".into(),
        _ => "trace".into(),
    };

    let mut config = Config::default();

    // Try loading config file.
    let config_path = cli.config.clone().unwrap_or_else(|| {
        let user = dirs_config().join("wolfram/config.toml");
        if user.exists() {
            return user;
        }
        PathBuf::from("/etc/wolfram/config.toml")
    });

    if config_path.exists() {
        let contents = std::fs::read_to_string(&config_path)
            .with_context(|| format!("reading config file {}", config_path.display()))?;
        let file: FileConfig = toml::from_str(&contents)
            .with_context(|| format!("parsing config file {}", config_path.display()))?;

        if let Some(v) = file.min_process_anon_mib {
            config.min_process_anon_mib = v;
        }
        if let Some(v) = file.min_region_kib {
            config.min_region_kib = v;
        }
        if let Some(v) = file.idle_sample_secs {
            config.idle_sample_secs = v;
        }
        if let Some(v) = file.cycle_secs {
            config.cycle_secs = v;
        }
        if let Some(v) = file.cold_threshold {
            config.cold_threshold = v;
        }
        if let Some(v) = file.pageout_threshold {
            config.pageout_threshold = v;
        }
        if let Some(v) = file.pageout_after_cycles {
            config.pageout_after_cycles = v;
        }
        if let Some(v) = file.enable_hibernation {
            config.enable_hibernation = v;
        }
        if let Some(v) = file.hibernate_after_cycles {
            config.hibernate_after_cycles = v;
        }
        if let Some(v) = file.checkpoint_dir {
            config.checkpoint_dir = PathBuf::from(v);
        }
        if let Some(v) = file.enable_port_proxy {
            config.enable_port_proxy = v;
        }
        if let Some(v) = file.excluded_names {
            config.excluded_names = v;
        }
        if let Some(v) = file.excluded_pids {
            config.excluded_pids = v;
        }
        if let Some(v) = file.dry_run {
            config.dry_run = v;
        }

        log::info!("Loaded config from {}", config_path.display());
    }

    // Apply CLI overrides (highest priority).
    if let Some(v) = cli.min_anon {
        config.min_process_anon_mib = v;
    }
    if let Some(v) = cli.min_region {
        config.min_region_kib = v;
    }
    if let Some(v) = cli.idle_sample {
        config.idle_sample_secs = v;
    }
    if let Some(v) = cli.cycle {
        config.cycle_secs = v;
    }
    if let Some(v) = cli.cold_threshold {
        config.cold_threshold = v;
    }
    if let Some(v) = cli.pageout_threshold {
        config.pageout_threshold = v;
    }
    if let Some(v) = cli.pageout_cycles {
        config.pageout_after_cycles = v;
    }
    if cli.hibernate {
        config.enable_hibernation = true;
    }
    if let Some(v) = cli.hibernate_cycles {
        config.hibernate_after_cycles = v;
    }
    if let Some(v) = cli.checkpoint_dir {
        config.checkpoint_dir = v;
    }
    if cli.port_proxy {
        config.enable_port_proxy = true;
    }
    if !cli.exclude.is_empty() {
        config.excluded_names.extend(cli.exclude);
    }
    if !cli.exclude_pid.is_empty() {
        config.excluded_pids.extend(cli.exclude_pid);
    }
    if cli.dry_run {
        config.dry_run = true;
    }

    // Always exclude ourselves.
    if !config.excluded_names.contains(&"wolfram".to_string()) {
        config.excluded_names.push("wolfram".into());
    }

    Ok((config, log_level))
}

fn dirs_config() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(dir)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config")
    } else {
        PathBuf::from("/etc")
    }
}

//! `wattson sim` — run a synthetic device.
//!
//! This is what makes the tool usable with no hardware at all: a device that speaks the real
//! wire protocol, over TCP or in-process, generating a realistic trace with firmware events
//! embedded in it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result};
use wattson_core::transport::{PipeTransport, Transport};
use wattson_core::units::{Duration as UnitDuration, SampleRate};
use wattson_core::uri::{SimProfile, SimSpec};
use wattson_sim::device::SimDevice;
use wattson_sim::engine::{FaultInjection, SimConfig};
use wattson_sim::profiles::Profile;
use wattson_sim::server::{ByteChannel, SimServer, run_device};

use crate::exit;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Address to listen on. Use port 0 to let the OS choose.
    #[arg(long, default_value = "127.0.0.1:9000")]
    pub listen: String,

    /// Which synthetic workload to generate.
    #[arg(long, default_value = "ble-sensor")]
    pub profile: String,

    /// Seed for the deterministic random stream.
    #[arg(long, default_value_t = 42)]
    pub seed: u64,

    /// Samples per second.
    #[arg(long, default_value = "50k")]
    pub rate: String,

    /// Stop each connection's device after this long.
    #[arg(long)]
    pub duration: Option<String>,

    /// Drop blocks and corrupt bytes, to exercise the host's recovery paths.
    #[arg(long)]
    pub inject_faults: bool,

    /// List the available profiles and exit.
    #[arg(long)]
    pub list_profiles: bool,
}

pub fn run(args: &Args, quiet: bool) -> Result<i32> {
    if args.list_profiles {
        println!("{:<24} DESCRIPTION", "PROFILE");
        for p in SimProfile::ALL {
            println!("{:<24} {}", p.name(), describe(p));
        }
        return Ok(exit::SUCCESS);
    }

    let rate: SampleRate = args.rate.parse()?;
    let duration = args
        .duration
        .as_deref()
        .map(str::parse::<UnitDuration>)
        .transpose()?
        .map(|d| d.to_std());

    let profile = Profile::by_name(&args.profile)
        .with_context(|| format!("unknown profile {:?}; try --list-profiles", args.profile))?;

    let mut config = SimConfig::new(profile);
    config.seed = args.seed;
    config.sample_rate_hz = rate.0;
    if args.inject_faults {
        config.faults = FaultInjection::NOISY;
    }

    let server = SimServer::bind(&args.listen, config)
        .with_context(|| format!("could not listen on {}", args.listen))?
        .with_duration(duration);
    let addr = server.local_addr()?;

    // Printed on stdout, unconditionally, because a test harness that asked for port 0 needs
    // to read the chosen port from somewhere.
    println!("listening on {addr}");
    if !quiet {
        eprintln!(
            "Synthetic device: profile {}, {} samples/s, seed {}{}",
            args.profile,
            rate.0,
            args.seed,
            if args.inject_faults {
                ", faults injected"
            } else {
                ""
            }
        );
        eprintln!("Capture from it with:");
        eprintln!("    wattson capture --device tcp://{addr} --duration 10s --out capture.pprof");
    }
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));

    server.serve(stop)?;
    Ok(exit::SUCCESS)
}

fn describe(p: SimProfile) -> &'static str {
    match p {
        SimProfile::BleSensor => "sleep, read a sensor, transmit; BLE_TX costs about 245 uJ",
        SimProfile::BleSensorRegressed => {
            "the same node with a 21% longer transmission, about 296 uJ"
        }
        SimProfile::LoraNode => "rare, long, high-current transmissions",
        SimProfile::AlwaysOn => "a flat baseline that never sleeps",
    }
}

/// Start a device on a background thread behind an in-memory pipe.
///
/// This is what `--device sim://...` resolves to, so a capture can be taken with no socket at
/// all.
pub fn in_process(spec: SimSpec) -> Result<Box<dyn Transport>> {
    let profile = Profile::by_name(spec.profile.name())
        .with_context(|| format!("unknown profile {}", spec.profile))?;

    let mut config = SimConfig::new(profile);
    config.seed = spec.seed;
    config.sample_rate_hz = spec.sample_rate_hz;
    if spec.faults {
        config.faults = FaultInjection::NOISY;
    }

    let (mut host, mut device_end) = PipeTransport::pair();
    host.set_read_timeout(StdDuration::from_millis(50))?;
    device_end.set_read_timeout(StdDuration::from_millis(2))?;

    let stop = Arc::new(AtomicBool::new(false));
    thread::spawn(move || {
        run_device(SimDevice::new(config), PipeChannel(device_end), stop);
    });

    Ok(Box::new(host))
}

/// Adapts the core's in-memory pipe to the simulator's channel trait.
///
/// The simulator deliberately does not depend on `wattson-core`, so the two are joined here,
/// in the binary that needs both.
struct PipeChannel(PipeTransport);

impl ByteChannel for PipeChannel {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf).map_err(std::io::Error::other)
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.0.write_all(buf).map_err(std::io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_profile_resolves_to_a_real_workload() {
        for p in SimProfile::ALL {
            assert!(
                Profile::by_name(p.name()).is_some(),
                "{} has no workload",
                p.name()
            );
            assert!(!describe(p).is_empty());
        }
    }

    /// `--device sim://...` must work without a socket, so tests and quick looks need no port.
    #[test]
    fn an_in_process_device_answers_a_handshake() {
        let transport = in_process(SimSpec::default()).unwrap();
        let mut session = wattson_core::session::Session::new(transport);
        let info = session.handshake(StdDuration::from_secs(5)).unwrap();
        assert_eq!(info.serial_str(), "SIM-0001");
        assert_eq!(info.timer_hz, 1_000_000);
    }
}

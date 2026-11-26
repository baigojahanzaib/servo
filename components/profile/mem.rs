/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Memory profiling functions.

use std::borrow::ToOwned;
use std::collections::HashMap;
use std::fs::File;
use std::thread;

use log::debug;
use profile_traits::mem::{
    MemoryReport, MemoryReportResult, ProfilerChan, ProfilerMsg, Report, Reporter, ReporterRequest,
    ReportsChan,
};
use servo_base::generic_channel::{self, GenericCallback, GenericReceiver};

use crate::system_reporter;

const LOG_FILE_VAR: &str = "UNTRACKED_LOG_FILE";

pub struct Profiler {
    /// The port through which messages are received.
    pub port: GenericReceiver<ProfilerMsg>,

    /// Registered memory reporters.
    reporters: HashMap<String, Reporter>,
}

impl Profiler {
    pub fn create() -> ProfilerChan {
        let (chan, port) = generic_channel::channel().unwrap();

        if servo_allocator::is_tracking_unmeasured() && std::env::var(LOG_FILE_VAR).is_err() {
            eprintln!("Allocation tracking is enabled but {LOG_FILE_VAR} is unset.");
            servo_allocator::disable_unmeasured_tracking();
        }

        // Always spawn the memory profiler. If there is no timer thread it won't receive regular
        // `Print` events, but it will still receive the other events.
        thread::Builder::new()
            .name("MemoryProfiler".to_owned())
            .spawn(move || {
                let mut mem_profiler = Profiler::new(port);
                mem_profiler.start();
            })
            .expect("Thread spawning failed");

        let mem_profiler_chan = ProfilerChan(chan);

        // Register the system memory reporter, which will run on its own thread. It never needs to
        // be unregistered, because as long as the memory profiler is running the system memory
        // reporter can make measurements.
        let callback = GenericCallback::new(|message| {
            let request: ReporterRequest = message.unwrap();
            system_reporter::collect_reports(request)
        })
        .expect("Could not create system reporter callback");
        mem_profiler_chan.send(ProfilerMsg::RegisterReporter(
            "system-main".to_owned(),
            Reporter(callback),
        ));

        mem_profiler_chan
    }

    pub fn new(port: GenericReceiver<ProfilerMsg>) -> Profiler {
        Profiler {
            port,
            reporters: HashMap::new(),
        }
    }

    pub fn start(&mut self) {
        while let Ok(msg) = self.port.recv() {
            if !self.handle_msg(msg) {
                break;
            }
        }
    }

    fn handle_msg(&mut self, msg: ProfilerMsg) -> bool {
        match msg {
            ProfilerMsg::RegisterReporter(name, reporter) => {
                eprintln!("[memprof] profiler received registration: {}", name);
                debug!("Registering memory reporter: {}", name);
                // Panic if it has already been registered.
                let name_clone = name.clone();
                match self.reporters.insert(name, reporter) {
                    None => true,
                    Some(_) => panic!("RegisterReporter: '{}' name is already in use", name_clone),
                }
            },

            ProfilerMsg::UnregisterReporter(name) => {
                eprintln!("[memprof] profiler received UNregistration: {}", name);
                debug!("Unregistering memory reporter: {}", name);
                // Panic if it hasn't previously been registered.
                match self.reporters.remove(&name) {
                    Some(_) => true,
                    None => panic!("UnregisterReporter: '{}' name is unknown", name),
                }
            },

            ProfilerMsg::Report(sender) => {
                let main_pid = std::process::id();

                let reports = self.collect_reports();
                // Turn the pid -> reports map into a vector and add the
                // hint to find the main process.
                let results: Vec<MemoryReport> = reports
                    .into_iter()
                    .map(|(pid, reports)| MemoryReport {
                        pid,
                        reports,
                        is_main_process: pid == main_pid,
                    })
                    .collect();
                let _ = sender.send(MemoryReportResult { results });

                if let Ok(value) = std::env::var(LOG_FILE_VAR) {
                    match File::create(&value) {
                        Ok(file) => {
                            servo_allocator::dump_unmeasured(file);
                        },
                        Err(error) => {
                            log::error!("Error creating log file: {error:?}");
                        },
                    }
                }
                true
            },
            ProfilerMsg::Exit => false,
        }
    }

    /// Returns a map of pid -> reports
    fn collect_reports(&self) -> HashMap<u32, Vec<Report>> {
        let mut result = HashMap::new();

        eprintln!("[memprof] collecting from {} reporters", self.reporters.len());
        // Feather: parallel collection with per-reporter timeout. Sending all
        // requests first lets reporters generate concurrently on their own
        // threads; the timeout keeps one hung reporter (e.g. a script thread
        // stuck in layout collection) from blocking the entire round forever.
        let mut pending = Vec::new();
        for (name, reporter) in self.reporters.iter() {
            eprintln!("[memprof] reporter: {}", name);
            let (chan, port) = generic_channel::channel().unwrap();
            reporter.collect_reports(ReportsChan(chan));
            pending.push((name.clone(), port));
        }
        // Round-robin poll all pending reporters under one global deadline:
        // round duration is bounded by the slowest responder, not the sum.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        let mut pending: Vec<(String, _)> = pending;
        while !pending.is_empty() && std::time::Instant::now() < deadline {
            let mut still_pending = Vec::new();
            for (name, port) in pending {
                match port.try_recv() {
                    Ok(mut reports) => {
                        eprintln!("[memprof] reporter {} responded with {} rows", name, reports.reports.len());
                        result
                            .entry(reports.pid)
                            .or_insert(vec![])
                            .append(&mut reports.reports);
                    },
                    Err(_) => still_pending.push((name, port)),
                }
            }
            pending = still_pending;
            if !pending.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        for (name, _) in pending {
            eprintln!("[memprof] reporter {} TIMED OUT — skipped", name);
        }
        result
    }
}

// Copyright 2018-2019 the Deno authors. All rights reserved. MIT license.
use crate::compiler::compile_async;
use crate::compiler::ModuleMetaData;
use crate::deno_dir;
use crate::errors::DenoError;
use crate::errors::DenoResult;
use crate::flags;
use crate::global_timer::GlobalTimer;
use crate::msg;
use crate::ops;
use crate::permissions::DenoPermissions;
use crate::progress::Progress;
use crate::resources;
use crate::resources::ResourceId;
use crate::tokio_util;
use crate::worker::resolve_module_spec;
use crate::worker::Worker;
use deno::Buf;
use deno::Loader;
use deno::Op;
use deno::PinnedBuf;
use futures::future::Either;
use futures::future::Shared;
use futures::Future;
use std;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;
use tokio::sync::mpsc as async_mpsc;

pub type WorkerSender = async_mpsc::Sender<Buf>;
pub type WorkerReceiver = async_mpsc::Receiver<Buf>;
pub type WorkerChannels = (WorkerSender, WorkerReceiver);
pub type UserWorkerTable = HashMap<ResourceId, Shared<Worker>>;

#[derive(Default)]
pub struct Metrics {
  pub ops_dispatched: AtomicUsize,
  pub ops_completed: AtomicUsize,
  pub bytes_sent_control: AtomicUsize,
  pub bytes_sent_data: AtomicUsize,
  pub bytes_received: AtomicUsize,
  pub resolve_count: AtomicUsize,
  pub compiler_starts: AtomicUsize,
}

/// Isolate cannot be passed between threads but ThreadSafeState can.
/// ThreadSafeState satisfies Send and Sync. So any state that needs to be
/// accessed outside the main V8 thread should be inside ThreadSafeState.
pub struct ThreadSafeState(Arc<State>);

#[cfg_attr(feature = "cargo-clippy", allow(stutter))]
pub struct State {
  pub dir: deno_dir::DenoDir,
  pub argv: Vec<String>,
  pub permissions: DenoPermissions,
  pub flags: flags::DenoFlags,
  /// When flags contains a `.config_path` option, the content of the
  /// configuration file will be resolved and set.
  pub config: Option<Vec<u8>>,
  /// When flags contains a `.config_path` option, the fully qualified path
  /// name of the passed path will be resolved and set.
  pub config_path: Option<String>,
  pub metrics: Metrics,
  pub worker_channels: Mutex<WorkerChannels>,
  pub global_timer: Mutex<GlobalTimer>,
  pub workers: Mutex<UserWorkerTable>,
  pub start_time: Instant,
  /// A reference to this worker's resource.
  pub resource: resources::Resource,
  pub dispatch_selector: ops::OpSelector,
  /// Reference to global progress bar.
  pub progress: Progress,

  /// Set of all URLs that have been compiled. This is a hacky way to work
  /// around the fact that --reload will force multiple compilations of the same
  /// module.
  compiled: Mutex<HashSet<String>>,
}

impl Clone for ThreadSafeState {
  fn clone(&self) -> Self {
    ThreadSafeState(self.0.clone())
  }
}

impl Deref for ThreadSafeState {
  type Target = Arc<State>;
  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

impl ThreadSafeState {
  pub fn dispatch(&self, control: &[u8], zero_copy: Option<PinnedBuf>) -> Op {
    ops::dispatch_all(self, control, zero_copy, self.dispatch_selector)
  }
}

pub fn fetch_module_meta_data_and_maybe_compile_async(
  state: &ThreadSafeState,
  specifier: &str,
  referrer: &str,
) -> impl Future<Item = ModuleMetaData, Error = DenoError> {
  let state_ = state.clone();
  let specifier = specifier.to_string();
  let referrer = referrer.to_string();

  let f =
    futures::future::result(ThreadSafeState::resolve(&specifier, &referrer));
  f.and_then(move |module_id| {
    let use_cache = !state_.flags.reload || state_.has_compiled(&module_id);
    let no_fetch = state_.flags.no_fetch;

    state_
      .dir
      .fetch_module_meta_data_async(&specifier, &referrer, use_cache, no_fetch)
      .and_then(move |out| {
        if out.media_type == msg::MediaType::TypeScript
          && !out.has_output_code_and_source_map()
        {
          debug!(">>>>> compile_sync START");
          Either::A(
            compile_async(state_.clone(), &out)
              .map_err(|e| {
                debug!("compiler error exiting!");
                eprintln!("\n{}", e.to_string());
                std::process::exit(1);
              }).and_then(move |out| {
                debug!(">>>>> compile_sync END");
                Ok(out)
              }),
          )
        } else {
          Either::B(futures::future::ok(out))
        }
      })
  })
}

pub fn fetch_module_meta_data_and_maybe_compile(
  state: &ThreadSafeState,
  specifier: &str,
  referrer: &str,
) -> Result<ModuleMetaData, DenoError> {
  tokio_util::block_on(fetch_module_meta_data_and_maybe_compile_async(
    state, specifier, referrer,
  ))
}

impl Loader for ThreadSafeState {
  type Error = DenoError;

  fn resolve(specifier: &str, referrer: &str) -> Result<String, Self::Error> {
    resolve_module_spec(specifier, referrer).map_err(DenoError::from)
  }

  /// Given an absolute url, load its source code.
  fn load(&self, url: &str) -> Box<deno::SourceCodeInfoFuture<Self::Error>> {
    self.metrics.resolve_count.fetch_add(1, Ordering::SeqCst);
    Box::new(
      fetch_module_meta_data_and_maybe_compile_async(self, url, ".")
        .map_err(|err| {
          eprintln!("{}", err);
          err
        }).map(|module_meta_data| deno::SourceCodeInfo {
          // Real module name, might be different from initial URL
          // due to redirections.
          code: module_meta_data.js_source(),
          module_name: module_meta_data.module_name,
        }),
    )
  }
}

impl ThreadSafeState {
  pub fn new(
    flags: flags::DenoFlags,
    argv_rest: Vec<String>,
    dispatch_selector: ops::OpSelector,
    progress: Progress,
  ) -> Self {
    let custom_root = env::var("DENO_DIR").map(String::into).ok();

    let (worker_in_tx, worker_in_rx) = async_mpsc::channel::<Buf>(1);
    let (worker_out_tx, worker_out_rx) = async_mpsc::channel::<Buf>(1);
    let internal_channels = (worker_out_tx, worker_in_rx);
    let external_channels = (worker_in_tx, worker_out_rx);
    let resource = resources::add_worker(external_channels);

    // take the passed flag and resolve the file name relative to the cwd
    let config_file = match &flags.config_path {
      Some(config_file_name) => {
        debug!("Compiler config file: {}", config_file_name);
        let cwd = std::env::current_dir().unwrap();
        Some(cwd.join(config_file_name))
      }
      _ => None,
    };

    // Convert the PathBuf to a canonicalized string.  This is needed by the
    // compiler to properly deal with the configuration.
    let config_path = match &config_file {
      Some(config_file) => Some(
        config_file
          .canonicalize()
          .unwrap()
          .to_str()
          .unwrap()
          .to_owned(),
      ),
      _ => None,
    };

    // Load the contents of the configuration file
    let config = match &config_file {
      Some(config_file) => {
        debug!("Attempt to load config: {}", config_file.to_str().unwrap());
        match fs::read(&config_file) {
          Ok(config_data) => Some(config_data.to_owned()),
          _ => panic!(
            "Error retrieving compiler config file at \"{}\"",
            config_file.to_str().unwrap()
          ),
        }
      }
      _ => None,
    };

    ThreadSafeState(Arc::new(State {
      dir: deno_dir::DenoDir::new(custom_root, &config, progress.clone())
        .unwrap(),
      argv: argv_rest,
      permissions: DenoPermissions::from_flags(&flags),
      flags,
      config,
      config_path,
      metrics: Metrics::default(),
      worker_channels: Mutex::new(internal_channels),
      global_timer: Mutex::new(GlobalTimer::new()),
      workers: Mutex::new(UserWorkerTable::new()),
      start_time: Instant::now(),
      resource,
      dispatch_selector,
      progress,
      compiled: Mutex::new(HashSet::new()),
    }))
  }

  /// Read main module from argv
  pub fn main_module(&self) -> Option<String> {
    if self.argv.len() <= 1 {
      None
    } else {
      let specifier = self.argv[1].clone();
      let referrer = ".";
      match self.dir.resolve_module_url(&specifier, referrer) {
        Ok(url) => Some(url.to_string()),
        Err(e) => {
          debug!("Potentially swallowed error {}", e);
          None
        }
      }
    }
  }

  pub fn mark_compiled(&self, module_id: &str) {
    let mut c = self.compiled.lock().unwrap();
    c.insert(module_id.to_string());
  }

  pub fn has_compiled(&self, module_id: &str) -> bool {
    let c = self.compiled.lock().unwrap();
    c.contains(module_id)
  }

  #[inline]
  pub fn check_read(&self, filename: &str) -> DenoResult<()> {
    self.permissions.check_read(filename)
  }

  #[inline]
  pub fn check_write(&self, filename: &str) -> DenoResult<()> {
    self.permissions.check_write(filename)
  }

  #[inline]
  pub fn check_env(&self) -> DenoResult<()> {
    self.permissions.check_env()
  }

  #[inline]
  pub fn check_net(&self, host_and_port: &str) -> DenoResult<()> {
    self.permissions.check_net(host_and_port)
  }

  #[inline]
  pub fn check_net_url(&self, url: url::Url) -> DenoResult<()> {
    self.permissions.check_net_url(url)
  }

  #[inline]
  pub fn check_run(&self) -> DenoResult<()> {
    self.permissions.check_run()
  }

  #[cfg(test)]
  pub fn mock() -> ThreadSafeState {
    let argv = vec![String::from("./deno"), String::from("hello.js")];
    ThreadSafeState::new(
      flags::DenoFlags::default(),
      argv,
      ops::op_selector_std,
      Progress::new(),
    )
  }

  pub fn metrics_op_dispatched(
    &self,
    bytes_sent_control: usize,
    bytes_sent_data: usize,
  ) {
    self.metrics.ops_dispatched.fetch_add(1, Ordering::SeqCst);
    self
      .metrics
      .bytes_sent_control
      .fetch_add(bytes_sent_control, Ordering::SeqCst);
    self
      .metrics
      .bytes_sent_data
      .fetch_add(bytes_sent_data, Ordering::SeqCst);
  }

  pub fn metrics_op_completed(&self, bytes_received: usize) {
    self.metrics.ops_completed.fetch_add(1, Ordering::SeqCst);
    self
      .metrics
      .bytes_received
      .fetch_add(bytes_received, Ordering::SeqCst);
  }
}

#[test]
fn thread_safe() {
  fn f<S: Send + Sync>(_: S) {}
  f(ThreadSafeState::mock());
}
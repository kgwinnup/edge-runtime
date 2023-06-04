use crate::deno_runtime::{DenoRuntime, EdgeCallResult};
use crate::utils::send_event_if_event_manager_available;
use crate::utils::units::bytes_to_display;

use anyhow::{anyhow, bail, Error};
use cityhash::cityhash_1_1_1::city_hash_64;
use deno_core::JsRuntime;
use hyper::{Body, Request, Response};
use log::{debug, error};
use sb_worker_context::essentials::{
    CreateUserWorkerResult, EdgeContextInitOpts, EdgeContextOpts, EdgeEventRuntimeOpts,
    UserWorkerMsgs,
};
use sb_worker_context::events::{BootEvent, BootFailure, UncaughtException, WorkerEvents};
use std::collections::HashMap;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::UnixStream;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug)]
pub struct WorkerRequestMsg {
    pub req: Request<Body>,
    pub res_tx: oneshot::Sender<Result<Response<Body>, hyper::Error>>,
}

#[derive(Debug, Clone)]
pub struct UserWorkerProfile {
    worker_event_tx: mpsc::UnboundedSender<WorkerRequestMsg>,
    event_manager_tx: Option<mpsc::UnboundedSender<WorkerEvents>>,
}

async fn handle_request(
    unix_stream_tx: mpsc::UnboundedSender<UnixStream>,
    msg: WorkerRequestMsg,
) -> Result<(), Error> {
    // create a unix socket pair
    let (sender_stream, recv_stream) = UnixStream::pair()?;

    let _ = unix_stream_tx.send(recv_stream);

    // send the HTTP request to the worker over Unix stream
    let (mut request_sender, connection) = hyper::client::conn::handshake(sender_stream).await?;

    // spawn a task to poll the connection and drive the HTTP state
    tokio::task::spawn(async move {
        if let Err(e) = connection.without_shutdown().await {
            error!("Error in worker connection: {}", e);
        }
    });
    tokio::task::yield_now().await;

    let result = request_sender.send_request(msg.req).await;
    let _ = msg.res_tx.send(result);

    Ok(())
}

struct TimerId(*mut libc::c_void);

#[cfg(target_os = "linux")]
impl Drop for TimerId {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { libc::timer_delete(self.0) };
        }
    }
}

fn get_thread_time() -> Result<i64, Error> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }

    Ok(time.tv_nsec)
}

struct CPUTimer {}

impl CPUTimer {
    #[cfg(target_os = "linux")]
    fn start(&self, thread_id: i32) -> Result<TimerId, Error> {
        let mut timerid = TimerId(std::ptr::null_mut());
        let mut sigev: libc::sigevent = unsafe { std::mem::zeroed() };
        sigev.sigev_notify = libc::SIGEV_THREAD_ID;
        sigev.sigev_signo = libc::SIGALRM;
        sigev.sigev_notify_thread_id = thread_id;

        if unsafe {
            // creates a new per-thread timer
            libc::timer_create(
                libc::CLOCK_THREAD_CPUTIME_ID,
                &mut sigev as *mut libc::sigevent,
                &mut timerid.0 as *mut *mut libc::c_void,
            )
        } < 0
        {
            bail!(std::io::Error::last_os_error())
        }

        let mut tmspec: libc::itimerspec = unsafe { std::mem::zeroed() };
        tmspec.it_interval.tv_sec = 0;
        tmspec.it_interval.tv_nsec = 10 * 1_000_000;
        tmspec.it_value.tv_sec = 0;
        tmspec.it_value.tv_nsec = 10 * 1_000_000;

        if unsafe {
            // start the timer with an expiry
            libc::timer_settime(timerid.0, 0, &tmspec, std::ptr::null_mut())
        } < 0
        {
            bail!(std::io::Error::last_os_error())
        }

        Ok(timerid)
    }

    #[cfg(not(target_os = "linux"))]
    fn start(&self, thread_id: i32) -> Result<TimerId, Box<dyn std::error::Error>> {
        println!("CPU timer: not enabled (need Linux)");
        Err(Box::new(&"not linux error"))
    }
}

#[cfg(target_os = "linux")]
fn get_thread_id() -> i32 {
    let tid;
    unsafe { tid = libc::gettid() }
    return tid;
}

#[cfg(not(target_os = "linux"))]
fn get_thread_id() -> i32 {
    return 0;
}

struct WorkerLimits {
    wall_clock_limit_ms: u64,
    low_memory_multiplier: u64,
    max_cpu_bursts: u64,
    cpu_burst_interval_ms: u128,
}

async fn create_supervisor(
    key: u64,
    js_runtime: &mut JsRuntime,
    force_quit_tx: oneshot::Sender<()>,
    worker_limits: WorkerLimits,
) -> Result<i32, Error> {
    let mut signals = signal(SignalKind::alarm())?;
    let (thread_id_tx, thread_id_rx) = oneshot::channel::<i32>();
    let thread_safe_handle = js_runtime.v8_isolate().thread_safe_handle();

    let (memory_limit_tx, mut memory_limit_rx) = mpsc::unbounded_channel::<()>();
    js_runtime.add_near_heap_limit_callback(move |cur, _| {
        debug!(
            "Low memory alert triggered: {}",
            bytes_to_display(cur as u64),
        );

        if memory_limit_tx.send(()).is_err() {
            error!("failed to send memory limit reached notification - isolate may already be terminating");
        };

        // give an allowance on current limit (until the isolate is terminated)
        // we do this so that oom won't end up killing the edge-runtime process
        cur * (worker_limits.low_memory_multiplier as usize)
    });

    let thread_name = format!("sb-sup-{:?}", key);
    let _handle = thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let local = tokio::task::LocalSet::new();
            thread_id_tx.send(get_thread_id()).unwrap();

            let future = async move {
                let mut bursts = 0;
                let mut last_burst = Instant::now();

                let sleep = tokio::time::sleep(Duration::from_millis(worker_limits.wall_clock_limit_ms));
                tokio::pin!(sleep);

                loop {
                    tokio::select! {
                        // handle the CPU time alarm
                        // FIME: multiple cpu alarms receiving
                        Some(_) = signals.recv() => {
                            if last_burst.elapsed().as_millis() > worker_limits.cpu_burst_interval_ms {
                                bursts += 1;
                                last_burst = Instant::now();
                            }
                            if bursts > worker_limits.max_cpu_bursts {
                                thread_safe_handle.terminate_execution();
                                error!("CPU time limit reached. isolate: {:?}", key);
                                return;
                            }
                        }

                        // wall-clock limit
                        () = &mut sleep => {
                            thread_safe_handle.terminate_execution();
                            error!("wall clock duration reached. isolate: {:?}", key);
                            return;

                        }

                        // memory usage
                        Some(_) = memory_limit_rx.recv() => {
                            thread_safe_handle.terminate_execution();
                            error!("memory limit reached for the worker. isolate: {:?}", key);
                            //send_event_if_event_manager_available(event_sender, WorkerEvents::MemoryLimit(PseudoEvent {}));
                            return;
                        }
                    }
                }
            };

            let _ = local.block_on(&rt, future);

            // send force quit signal
            let _ = force_quit_tx.send(());
        })
        .unwrap();

    let thread_id = thread_id_rx.await?;
    Ok(thread_id)
}

pub async fn create_worker(
    init_opts: EdgeContextInitOpts,
    event_manager_opts: Option<EdgeEventRuntimeOpts>,
) -> Result<mpsc::UnboundedSender<WorkerRequestMsg>, Error> {
    let service_path = init_opts.service_path.clone();

    if !service_path.exists() {
        bail!("service does not exist {:?}", &service_path)
    }

    let (worker_boot_result_tx, worker_boot_result_rx) = oneshot::channel::<Result<(), Error>>();
    let (unix_stream_tx, unix_stream_rx) = mpsc::unbounded_channel::<UnixStream>();

    let (worker_key, pool_msg_tx, event_msg_tx, thread_name) = match init_opts.conf.clone() {
        EdgeContextOpts::UserWorker(worker_opts) => (
            worker_opts.key,
            worker_opts.pool_msg_tx,
            worker_opts.events_msg_tx,
            worker_opts
                .key
                .map(|k| format!("sb-iso-{:?}", k))
                .unwrap_or("isolate-worker-unknown".to_string()),
        ),
        EdgeContextOpts::MainWorker(_) => (None, None, None, "main-worker".to_string()),
        EdgeContextOpts::EventsWorker => (None, None, None, "events-worker".to_string()),
    };

    // spawn a thread to run the worker
    let _handle: thread::JoinHandle<Result<(), Error>> = thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let local = tokio::task::LocalSet::new();

            let result: Result<EdgeCallResult, Error> = local.block_on(&runtime, async {
                match DenoRuntime::new(init_opts, event_manager_opts).await {
                    Err(err) => {
                        let _ = worker_boot_result_tx.send(Err(anyhow!("worker boot error")));
                        bail!(err)
                    }
                    Ok(mut worker) => {
                        let _ = worker_boot_result_tx.send(Ok(()));

                        let (force_quit_tx, force_quit_rx) = oneshot::channel::<()>();

                        // start CPU timer only if the worker is a user worker
                        //let mut timerid = None;
                        if worker.is_user_runtime {
                            let start_time = get_thread_time();
                            println!("start time {:?}", start_time);

                            let wall_clock_limit_ms = 60 * 1000;
                            let low_memory_multiplier = 5;
                            let max_cpu_bursts = 10;
                            let cpu_burst_interval_ms = 100;

                            let thread_id = create_supervisor(
                                worker_key.unwrap_or(0),
                                &mut worker.js_runtime,
                                force_quit_tx,
                                WorkerLimits {
                                    wall_clock_limit_ms,
                                    low_memory_multiplier,
                                    max_cpu_bursts,
                                    cpu_burst_interval_ms,
                                },
                            )
                            .await?;
                            //let cpu_timer = CPUTimer {};
                            //// Note: we intentionally let the thread to panic here if CPU timer cannot be started
                            //timerid = Some(cpu_timer.start(thread_id).unwrap());
                        }

                        worker.run(unix_stream_rx, force_quit_rx).await
                    }
                }
            });

            if let Err(err) = result {
                send_event_if_event_manager_available(
                    event_msg_tx,
                    WorkerEvents::UncaughtException(UncaughtException {
                        exception: err.to_string(),
                    }),
                );
                error!("worker {:?} returned an error: {:?}", service_path, err);
            }

            let end_time = get_thread_time();
            println!("end time {:?}", end_time);

            // remove the worker from pool
            if let Some(k) = worker_key {
                if let Some(tx) = pool_msg_tx {
                    let res = tx.send(UserWorkerMsgs::Shutdown(k));
                    if res.is_err() {
                        error!(
                            "failed to send the shutdown signal to user worker pool: {:?}",
                            res.unwrap_err()
                        );
                    }
                }
            }

            Ok(())
        })
        .unwrap();

    // create an async task waiting for requests for worker
    let (worker_req_tx, mut worker_req_rx) = mpsc::unbounded_channel::<WorkerRequestMsg>();

    let worker_req_handle: tokio::task::JoinHandle<Result<(), Error>> =
        tokio::task::spawn(async move {
            while let Some(msg) = worker_req_rx.recv().await {
                let unix_stream_tx_clone = unix_stream_tx.clone();
                tokio::task::spawn(async move {
                    if let Err(err) = handle_request(unix_stream_tx_clone, msg).await {
                        error!("worker failed to handle request: {:?}", err);
                    }
                });
            }

            Ok(())
        });

    // wait for worker to be successfully booted
    let worker_boot_result = worker_boot_result_rx.await?;
    match worker_boot_result {
        Err(err) => {
            worker_req_handle.abort();
            bail!(err)
        }
        Ok(_) => Ok(worker_req_tx),
    }
}

async fn send_user_worker_request(
    worker_channel: mpsc::UnboundedSender<WorkerRequestMsg>,
    req: Request<Body>,
) -> Result<Response<Body>, Error> {
    let (res_tx, res_rx) = oneshot::channel::<Result<Response<Body>, hyper::Error>>();
    let msg = WorkerRequestMsg { req, res_tx };

    // send the message to worker
    worker_channel.send(msg)?;

    // wait for the response back from the worker
    let res = res_rx.await??;

    // send the response back to the caller

    Ok(res)
}

pub async fn create_event_worker(
    event_worker_path: PathBuf,
    import_map_path: Option<String>,
    no_module_cache: bool,
) -> Result<mpsc::UnboundedSender<WorkerEvents>, Error> {
    let (event_tx, event_rx) = mpsc::unbounded_channel::<WorkerEvents>();

    let _ = create_worker(
        EdgeContextInitOpts {
            service_path: event_worker_path,
            no_module_cache,
            import_map_path,
            env_vars: std::env::vars().collect(),
            conf: EdgeContextOpts::EventsWorker,
        },
        Some(EdgeEventRuntimeOpts { event_rx }),
    )
    .await?;

    Ok(event_tx)
}

pub async fn create_user_worker_pool(
    worker_event_sender: Option<mpsc::UnboundedSender<WorkerEvents>>,
) -> Result<mpsc::UnboundedSender<UserWorkerMsgs>, Error> {
    let (user_worker_msgs_tx, mut user_worker_msgs_rx) =
        mpsc::unbounded_channel::<UserWorkerMsgs>();

    let user_worker_msgs_tx_clone = user_worker_msgs_tx.clone();
    let _handle: tokio::task::JoinHandle<Result<(), Error>> = tokio::spawn(async move {
        let mut user_workers: HashMap<u64, UserWorkerProfile> = HashMap::new();

        loop {
            match user_worker_msgs_rx.recv().await {
                None => break,
                Some(UserWorkerMsgs::Create(mut worker_options, tx)) => {
                    let mut user_worker_rt_opts = match worker_options.conf {
                        EdgeContextOpts::UserWorker(opts) => opts,
                        _ => unreachable!(),
                    };

                    // derive worker key from service path
                    // if force create is set, add current epoch mili seconds to randomize
                    let service_path = worker_options.service_path.to_str().unwrap_or("");
                    let mut key_input = service_path.to_string();
                    if user_worker_rt_opts.force_create {
                        let cur_epoch_time = SystemTime::now().duration_since(UNIX_EPOCH)?;
                        key_input = format!("{}-{}", key_input, cur_epoch_time.as_millis());
                    }
                    let key = city_hash_64(key_input.as_bytes());

                    // do not recreate the worker if it already exists
                    // unless force_create option is set
                    if !user_worker_rt_opts.force_create {
                        if let Some(_worker) = user_workers.get(&key) {
                            if tx.send(Ok(CreateUserWorkerResult { key })).is_err() {
                                bail!("main worker receiver dropped")
                            }
                            continue;
                        }
                    }

                    user_worker_rt_opts.key = Some(key);
                    user_worker_rt_opts.pool_msg_tx = Some(user_worker_msgs_tx_clone.clone());
                    user_worker_rt_opts.events_msg_tx = worker_event_sender.clone();
                    worker_options.conf = EdgeContextOpts::UserWorker(user_worker_rt_opts);
                    let now = Instant::now();
                    let result = create_worker(worker_options, None).await;
                    let elapsed = now.elapsed().as_secs();

                    let event_manager = worker_event_sender.clone();

                    match result {
                        Ok(user_worker_req_tx) => {
                            send_event_if_event_manager_available(
                                event_manager.clone(),
                                WorkerEvents::Boot(BootEvent {
                                    boot_time: elapsed as usize,
                                }),
                            );

                            user_workers.insert(
                                key,
                                UserWorkerProfile {
                                    worker_event_tx: user_worker_req_tx,
                                    event_manager_tx: event_manager,
                                },
                            );
                            if tx.send(Ok(CreateUserWorkerResult { key })).is_err() {
                                bail!("main worker receiver dropped")
                            };
                        }
                        Err(e) => {
                            send_event_if_event_manager_available(
                                event_manager,
                                WorkerEvents::BootFailure(BootFailure { msg: e.to_string() }),
                            );
                            if tx.send(Err(e)).is_err() {
                                bail!("main worker receiver dropped")
                            };
                        }
                    }
                }
                Some(UserWorkerMsgs::SendRequest(key, req, tx)) => {
                    match user_workers.get(&key) {
                        Some(worker) => {
                            let profile = worker.clone();
                            tokio::task::spawn(async move {
                                let req =
                                    send_user_worker_request(profile.worker_event_tx, req).await;
                                let result = match req {
                                    Ok(rep) => Ok(rep),
                                    Err(err) => {
                                        send_event_if_event_manager_available(
                                            profile.event_manager_tx,
                                            WorkerEvents::UncaughtException(UncaughtException {
                                                exception: err.to_string(),
                                            }),
                                        );
                                        Err(err)
                                    }
                                };

                                if tx.send(result).is_err() {
                                    error!("main worker receiver dropped")
                                }
                            });
                        }

                        None => {
                            if tx.send(Err(anyhow!("user worker not available"))).is_err() {
                                bail!("main worker receiver dropped")
                            }
                        }
                    };
                }
                Some(UserWorkerMsgs::Shutdown(key)) => {
                    user_workers.remove(&key);
                }
            }
        }

        Ok(())
    });

    Ok(user_worker_msgs_tx)
}

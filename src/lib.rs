use std::marker::PhantomData;
use std::process::exit;

use ipc_channel::ipc::{self, IpcOneShotServer, IpcReceiver, IpcSender};
use nix::unistd::{ForkResult, fork};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Decode(#[from] rmp_serde::decode::Error),
    #[error(transparent)]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("fork failed: {0}")]
    Fork(nix::errno::Errno),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Ipc(#[from] ipc_channel::Error),
    #[error("failed to spawn worker: {0}")]
    Spawn(String),
    #[error("failed to handle task: {0}")]
    Task(String),
}

#[derive(serde::Deserialize, serde::Serialize)]
pub enum SpawnRequest {
    SpawnWorker(String),
    Shutdown,
}

pub type SpawnResponse = Result<String, String>;

#[derive(serde::Deserialize, serde::Serialize)]
pub enum WorkerRequest<T> {
    Task(String, T),
    Shutdown,
}

pub type WorkerResponse<T> = Result<T, String>;

pub struct Spawner<W> {
    tx: IpcSender<SpawnRequest>,
    _phantom: PhantomData<W>,
}

pub struct Worker<Req, Resp> {
    tx: IpcSender<WorkerRequest<Vec<u8>>>,
    _marker: PhantomData<(Req, Resp)>,
}

pub trait WorkerTaskHandler {
    type Req: serde::Serialize + for<'de> serde::de::Deserialize<'de>;
    type Resp: serde::Serialize + for<'de> serde::de::Deserialize<'de>;

    type Error: std::error::Error;

    fn init() -> Result<Self, Self::Error>
    where
        Self: Sized;

    fn handle_task(&mut self, task: Self::Req) -> Result<Self::Resp, Self::Error>;
}

#[cfg(not(target_os = "macos"))]
#[inline]
fn new_one_shot_server<T>() -> Result<(IpcOneShotServer<T>, String), Error>
where
    T: serde::Serialize + for<'de> serde::de::Deserialize<'de>,
{
    IpcOneShotServer::<T>::new().map_err(Error::from)
}

#[cfg(target_os = "macos")]
#[inline]
fn new_one_shot_server<T>() -> Result<(IpcOneShotServer<T>, String), Error>
where
    T: serde::Serialize + for<'de> serde::de::Deserialize<'de>,
{
    // NOTE: we cannot properly handle MachError::Unknown(1100) since the
    // error type is not exported, so we attempt to handle the error which
    // appears due to a bad or clashing port name by reseeding the random
    // number generator and retrying the operation.
    //
    // The workaround appears to be unnecessary in the current (unreleased)
    // version of the crate.
    //
    match IpcOneShotServer::<T>::new() {
        Ok(srv_pn) => Ok(srv_pn),
        Err(e) => {
            if e.kind() != std::io::ErrorKind::Other {
                return Err(Error::from(e));
            }

            // NOTE: this is a workaround for the issue above.
            rand::rng().reseed().ok();
            IpcOneShotServer::<T>::new().map_err(Error::from)
        }
    }
}

impl<W> Spawner<W>
where
    W: WorkerTaskHandler,
{
    pub fn new() -> Result<Self, Error> {
        let (psrv, pn) = IpcOneShotServer::<String>::new()?;

        match unsafe { fork() }.map_err(Error::Fork)? {
            ForkResult::Child => {
                rand::rng().reseed().ok();

                let (csrv, cn) = new_one_shot_server::<IpcReceiver<SpawnRequest>>()?;

                let ctrl = IpcSender::connect(pn)?;

                ctrl.send(cn)?;

                let (_, task_rx) = csrv.accept()?;

                while let Ok(task) = task_rx.recv() {
                    match task {
                        SpawnRequest::SpawnWorker(srv) => {
                            Self::spawn_worker(srv)?;
                        }
                        SpawnRequest::Shutdown => {
                            break;
                        }
                    }
                }

                exit(0);
            }
            ForkResult::Parent { .. } => {
                let (_, cn) = psrv.accept()?;

                let ctrl = IpcSender::<IpcReceiver<SpawnRequest>>::connect(cn)?;

                let (tx, task_rx) = ipc::channel()?;

                ctrl.send(task_rx)?;

                Ok(Spawner {
                    tx,
                    _phantom: PhantomData,
                })
            }
        }
    }

    fn spawn_worker(srv: String) -> Result<(), Error> {
        match unsafe { fork() }.map_err(Error::Fork)? {
            ForkResult::Child => {
                rand::rng().reseed().ok();

                let (csrv, cn) = new_one_shot_server::<IpcReceiver<WorkerRequest<Vec<u8>>>>()?;

                let ctrl = IpcSender::connect(srv)?;

                let mut worker = match W::init() {
                    Ok(w) => w,
                    Err(e) => {
                        ctrl.send(Err(e.to_string()))?;
                        exit(-1);
                    }
                };

                ctrl.send(Ok(cn))?;

                let (_, task_rx) = csrv.accept()?;

                while let Ok(task) = task_rx.recv() {
                    match task {
                        WorkerRequest::Task(rsrv, task) => {
                            let ctrl = IpcSender::<WorkerResponse<Vec<u8>>>::connect(rsrv)?;
                            match rmp_serde::decode::from_slice::<W::Req>(&task) {
                                Ok(req) => {
                                    let resp = worker
                                        .handle_task(req)
                                        .map_err(|e| e.to_string())
                                        .and_then(|resp| {
                                            rmp_serde::encode::to_vec(&resp)
                                                .map_err(|e| e.to_string())
                                        });
                                    ctrl.send(resp)?;
                                }
                                Err(e) => {
                                    ctrl.send(Err(e.to_string()))?;
                                }
                            }
                        }
                        WorkerRequest::Shutdown => {
                            break;
                        }
                    }
                }

                exit(0);
            }
            ForkResult::Parent { .. } => {}
        }
        Ok(())
    }

    pub fn spawn(&self) -> Result<Worker<W::Req, W::Resp>, Error> {
        let (psrv, pn) = new_one_shot_server::<SpawnResponse>()?;

        self.tx.send(SpawnRequest::SpawnWorker(pn))?;

        let (_, resp) = psrv.accept()?;

        let csrv = resp.map_err(Error::Spawn)?;

        let ctrl = IpcSender::<IpcReceiver<WorkerRequest<Vec<u8>>>>::connect(csrv)?;

        let (tx, task_rx) = ipc::channel()?;

        ctrl.send(task_rx)?;

        Ok(Worker {
            tx,
            _marker: PhantomData,
        })
    }
}

impl<W> Spawner<W> {
    pub fn shutdown(&mut self) -> Result<(), Error> {
        self.tx.send(SpawnRequest::Shutdown)?;
        Ok(())
    }
}

impl<W> Drop for Spawner<W> {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

impl<Req, Resp> Worker<Req, Resp>
where
    Req: serde::Serialize + for<'de> serde::de::Deserialize<'de>,
    Resp: serde::Serialize + for<'de> serde::de::Deserialize<'de>,
{
    pub fn execute(&self, task: &Req) -> Result<Resp, Error> {
        let (psrv, pn) = new_one_shot_server::<WorkerResponse<Vec<u8>>>()?;

        let req = rmp_serde::encode::to_vec(&task)?;

        self.tx.send(WorkerRequest::Task(pn, req))?;

        let (_, wresp) = psrv.accept()?;

        let encoded = wresp.map_err(Error::Task)?;
        let resp = rmp_serde::decode::from_slice(&encoded)?;

        Ok(resp)
    }
}

impl<Req, Resp> Worker<Req, Resp> {
    fn shutdown(&mut self) -> Result<(), Error> {
        self.tx.send(WorkerRequest::Shutdown)?;
        Ok(())
    }
}

impl<Req, Resp> Drop for Worker<Req, Resp> {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use super::*;

    #[derive(Debug, Clone)]
    struct TestWorker {
        count: Arc<Mutex<u32>>,
    }

    impl WorkerTaskHandler for TestWorker {
        type Req = String;
        type Resp = String;
        type Error = std::io::Error;

        fn init() -> Result<Self, Self::Error> {
            Ok(TestWorker {
                count: Arc::new(Mutex::new(0)),
            })
        }

        fn handle_task(&mut self, task: Self::Req) -> Result<Self::Resp, Self::Error> {
            let mut count = self.count.lock().unwrap();
            *count += 1;
            Ok(format!("Task {} handled. Count: {}", task, *count))
        }
    }

    #[test]
    fn test_spawner() -> Result<(), Error> {
        let spawner = Spawner::<TestWorker>::new()?;
        let worker = spawner.spawn()?;

        let task = "Test Task".to_string();
        let response = worker.execute(&task)?;

        assert_eq!(response, format!("Task {} handled. Count: 1", task));

        Ok(())
    }
}

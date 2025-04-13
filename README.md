# spawn

Simple IPC-based RPC for forked processes.

## Usage

```rust
use std::sync::{Arc, Mutex};
use spawn::*;

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

fn main() {
    let spawner = Spawner::<TestWorker>::new()?;
    let worker = spawner.spawn()?;

    let task = "Test Task".to_string();
    let response = worker.execute(&task)?;

    assert_eq!(response, format!("Task {} handled. Count: 1", task));

    Ok(())
}
```

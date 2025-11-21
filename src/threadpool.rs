use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

type Job = Box<dyn FnOnce() + Send + 'static>;

pub struct ThreadPool {
    _workers: Vec<Worker>,
    tx: mpsc::Sender<Job>,
}

impl ThreadPool {
    pub fn new(num_workers: usize) -> Self {
        assert!(num_workers < 10);

        let (tx, rx) = mpsc::channel();
        let rx = Arc::new(Mutex::new(rx));

        let mut workers = Vec::with_capacity(num_workers);
        for i in 0..num_workers {
            workers.push(Worker::new(i, Arc::clone(&rx)));
        }

        Self {
            _workers: workers,
            tx,
        }
    }

    pub fn execute(&self, job: impl FnOnce() + Send + 'static) {
        let job = Box::new(job);

        self.tx.send(job).unwrap();
    }
}

struct Worker {
    _id: usize,
    _thread: thread::JoinHandle<()>,
}

impl Worker {
    fn new(id: usize, task_channel: Arc<Mutex<mpsc::Receiver<Job>>>) -> Self {
        println!("Spawning worker thread {id}");
        let thread = thread::spawn(move || {
            loop {
                let job = task_channel.lock().unwrap().recv().unwrap();
                println!("Executing job on worker thread {id}");

                job();
            }
        });

        Self {
            _id: id,
            _thread: thread,
        }
    }
}

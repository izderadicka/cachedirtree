use std::sync::{Arc, Condvar, Mutex};

#[derive(Clone)]
pub(crate) struct Cond(Arc<(Mutex<bool>, Condvar)>);
impl Cond {
    pub fn new() -> Self {
        Cond(Arc::new((Mutex::new(false), Condvar::new())))
    }
    
    pub fn notify(&self) {
        let mut x = (self.0).0.lock().unwrap();
        *x = true;
        (self.0).1.notify_one();
    }
    
    pub fn wait(&self) {
        let mut x = (self.0).0.lock().unwrap();
        while !*x {
            x = (self.0).1.wait(x).unwrap();
        }
        *x = false;
    }
}
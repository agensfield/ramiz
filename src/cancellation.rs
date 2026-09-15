use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

static CANCELLED: OnceLock<Arc<AtomicBool>> = OnceLock::new();

pub fn install() -> Result<(), std::io::Error> {
    if CANCELLED.get().is_some() {
        return Ok(());
    }
    let flag = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&flag))?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&flag))?;
    let _ = CANCELLED.set(flag);
    Ok(())
}

pub fn requested() -> bool {
    CANCELLED
        .get()
        .is_some_and(|flag| flag.load(Ordering::Relaxed))
}

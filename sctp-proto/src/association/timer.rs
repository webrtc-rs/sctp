use std::time::Instant;

const TIMER_COUNT: usize = 6;

#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq)]
pub(crate) enum Timer {
    T1Init = 0,
    T1Cookie = 1,
    T2Shutdown = 2,
    T3RTX = 3,
    Reconfig = 4,
    Ack = 5,
}

impl Timer {
    pub(crate) const VALUES: [Self; TIMER_COUNT] = [
        Timer::T1Init,
        Timer::T1Cookie,
        Timer::T2Shutdown,
        Timer::T3RTX,
        Timer::Reconfig,
        Timer::Ack,
    ];
}

/// A table of data associated with each distinct kind of `Timer`
#[derive(Debug, Copy, Clone, Default)]
pub(crate) struct TimerTable {
    data: [Option<Instant>; TIMER_COUNT],
}

impl TimerTable {
    pub fn set(&mut self, timer: Timer, time: Instant) {
        self.data[timer as usize] = Some(time);
    }

    pub fn get(&self, timer: Timer) -> Option<Instant> {
        self.data[timer as usize]
    }

    pub fn stop(&mut self, timer: Timer) {
        self.data[timer as usize] = None;
    }

    pub fn next_timeout(&self) -> Option<Instant> {
        self.data.iter().filter_map(|&x| x).min()
    }

    pub fn is_expired(&self, timer: Timer, after: Instant) -> bool {
        self.data[timer as usize].map_or(false, |x| x <= after)
    }
}

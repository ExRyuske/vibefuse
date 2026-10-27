//! The ASIO stream on Windows, run on the window's thread like the driver's
//! COM object wants.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use super::{Output, Status};
use crate::fx::asio::{self, Stream};

/// Pause before trying again after the stream failed to start (another
/// program may hold the driver).
const RETRY: Duration = Duration::from_secs(10);

#[derive(Default)]
struct State {
    wanted: Option<String>,
    stream: Option<Stream>,
    outputs: Vec<Output>,
    problem: Option<String>,
    retry: Option<Instant>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

pub fn set(output: Option<&str>) {
    STATE.with_borrow_mut(|state| {
        if state.wanted.as_deref() != output {
            state.wanted = output.map(str::to_owned);
            state.stream = None;
            state.retry = None;
        }
        apply(state);
    });
}

/// Starts the stream again when the driver asks for it or after a failure.
pub fn tick() {
    STATE.with_borrow_mut(|state| {
        if state.stream.as_ref().is_some_and(Stream::needs_restart) {
            state.stream = None;
            state.retry = None;
        }
        apply(state);
    });
}

fn apply(state: &mut State) {
    let Some(wanted) = state.wanted.clone() else {
        state.stream = None;
        state.problem = None;
        return;
    };
    if state.stream.is_some() || state.retry.is_some_and(|at| Instant::now() < at) {
        return;
    }
    let first = wanted.parse().unwrap_or(0);
    match Stream::start(first) {
        Ok(stream) => {
            state.stream = Some(stream);
            state.problem = None;
            state.retry = None;
        }
        Err(error) => {
            state.problem = Some(error);
            state.retry = Some(Instant::now() + RETRY);
        }
    }
}

/// Reads the driver's outputs again; while the stream runs, the driver is
/// ours and the list stays as it was.
pub fn refresh() {
    STATE.with_borrow_mut(|state| {
        if state.stream.is_some() {
            return;
        }
        match asio::outputs() {
            Ok(outputs) => {
                state.outputs = outputs
                    .into_iter()
                    .map(|(first, name)| Output {
                        id: first.to_string(),
                        name,
                    })
                    .collect();
            }
            Err(error) => state.problem = Some(error),
        }
    });
}

pub fn outputs() -> Vec<Output> {
    STATE.with_borrow(|state| state.outputs.clone())
}

pub fn status() -> Status {
    STATE.with_borrow(|state| Status {
        rate: state.stream.as_ref().map(Stream::rate),
        problem: state.problem.clone(),
    })
}

//! Raw-mode and alternate-screen ownership, including partial initialization.
use crossterm::{
    cursor::{Hide, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, Write};

pub(super) trait Modes {
    fn enable(&mut self) -> io::Result<()>;
    fn disable(&mut self) -> io::Result<()>;
}
pub(super) struct RealModes;
impl Modes for RealModes {
    fn enable(&mut self) -> io::Result<()> {
        crossterm::terminal::enable_raw_mode()
    }
    fn disable(&mut self) -> io::Result<()> {
        crossterm::terminal::disable_raw_mode()
    }
}

pub(super) struct Guard<W: Write, M: Modes = RealModes> {
    writer: W,
    modes: M,
    active: bool,
}
impl Guard<io::Stdout> {
    pub fn acquire() -> io::Result<Self> {
        Self::enter(io::stdout(), RealModes)
    }
}
impl<W: Write, M: Modes> Guard<W, M> {
    fn enter(writer: W, modes: M) -> io::Result<Self> {
        let mut guard = Self {
            writer,
            modes,
            active: true,
        };
        guard.modes.enable()?;
        execute!(
            guard.writer,
            EnterAlternateScreen,
            EnableBracketedPaste,
            Hide
        )?;
        Ok(guard)
    }
    pub fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        // Attempt every restoration even if a prior write failed. In particular
        // a broken stdout must never prevent resetting the terminal driver.
        let cursor = execute!(self.writer, Show);
        let paste = execute!(self.writer, DisableBracketedPaste);
        let screen = execute!(self.writer, LeaveAlternateScreen);
        let raw = self.modes.disable();
        cursor.and(paste).and(screen).and(raw)
    }
}
impl<W: Write, M: Modes> Drop for Guard<W, M> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    #[derive(Default)]
    struct State {
        bytes: Vec<u8>,
        writes: usize,
        fail: Option<usize>,
        raw: bool,
        resets: usize,
        fail_raw: bool,
    }
    #[derive(Clone)]
    struct Probe(Rc<RefCell<State>>);
    impl Write for Probe {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let mut s = self.0.borrow_mut();
            s.writes += 1;
            if s.fail == Some(s.writes) {
                return Err(io::Error::other("original write failure"));
            }
            s.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Modes for Probe {
        fn enable(&mut self) -> io::Result<()> {
            let mut s = self.0.borrow_mut();
            s.raw = true;
            if s.fail_raw {
                Err(io::Error::other("raw setup failed"))
            } else {
                Ok(())
            }
        }
        fn disable(&mut self) -> io::Result<()> {
            let mut s = self.0.borrow_mut();
            s.raw = false;
            s.resets += 1;
            Ok(())
        }
    }
    #[test]
    fn normal_error_and_unwind_restore_all_modes() {
        for mode in 0..3 {
            let state = Rc::new(RefCell::new(State::default()));
            let probe = Probe(state.clone());
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> io::Result<()> {
                    let mut guard = Guard::enter(probe.clone(), probe).unwrap();
                    assert!(state.borrow().raw);
                    match mode {
                        0 => guard.restore(),
                        1 => Err(io::Error::other("draw failure")),
                        _ => panic!("render unwind"),
                    }
                }));
            assert_eq!(result.is_err(), mode == 2);
            let s = state.borrow();
            assert!(!s.raw);
            assert_eq!(s.resets, 1);
            let bytes = String::from_utf8_lossy(&s.bytes);
            for reset in ["\x1b[?25h", "\x1b[?2004l", "\x1b[?1049l"] {
                assert!(bytes.contains(reset));
            }
        }
    }
    #[test]
    fn each_partial_setup_failure_restores_raw_mode() {
        for fail in 1..=3 {
            let state = Rc::new(RefCell::new(State {
                fail: Some(fail),
                ..State::default()
            }));
            let probe = Probe(state.clone());
            assert!(Guard::enter(probe.clone(), probe).is_err());
            let s = state.borrow();
            assert!(!s.raw);
            assert_eq!(s.resets, 1);
            assert!(String::from_utf8_lossy(&s.bytes).contains("\x1b[?1049l"));
        }
        let state = Rc::new(RefCell::new(State {
            fail_raw: true,
            ..State::default()
        }));
        let probe = Probe(state.clone());
        assert!(Guard::enter(probe.clone(), probe).is_err());
        assert!(!state.borrow().raw);
    }
    #[test]
    fn cleanup_write_failure_still_restores_terminal_driver() {
        let state = Rc::new(RefCell::new(State::default()));
        let probe = Probe(state.clone());
        let mut guard = Guard::enter(probe.clone(), probe).unwrap();
        let next = state.borrow().writes + 1;
        state.borrow_mut().fail = Some(next);
        assert!(guard.restore().is_err());
        assert!(!state.borrow().raw);
        assert!(String::from_utf8_lossy(&state.borrow().bytes).contains("\x1b[?1049l"));
    }
}

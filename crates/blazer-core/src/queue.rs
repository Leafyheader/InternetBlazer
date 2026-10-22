//! Download queues and the scheduler that starts and stops them.

use std::time::Duration;

use chrono::{Datelike, Local, NaiveDateTime, NaiveTime, TimeDelta};
use serde::{Deserialize, Serialize};

use crate::{engine::Engine, types::QueueId};

/// A queue starts its downloads in the order they were added, running at most `max_active` at a
/// time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Queue {
    pub id: QueueId,
    pub name: String,
    pub max_active: usize,
    #[serde(default)]
    pub schedule: Schedule,
}

impl Queue {
    pub(crate) fn main() -> Self {
        Self {
            id: QueueId::MAIN,
            name: "Main queue".into(),
            max_active: 3,
            schedule: Schedule::default(),
        }
    }
}

/// When a queue starts and stops by itself, in local time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Schedule {
    pub enabled: bool,
    /// Minutes after midnight.
    pub start: Option<u16>,
    /// Minutes after midnight. A stop at or before the start time is on the next day.
    pub stop: Option<u16>,
    /// Days the schedule runs on: bit 0 is Monday, bit 6 is Sunday.
    pub days: u8,
}

impl Default for Schedule {
    fn default() -> Self {
        Self { enabled: false, start: None, stop: None, days: ALL_DAYS }
    }
}

pub const ALL_DAYS: u8 = 0x7f;
const MINUTES_PER_DAY: u16 = 24 * 60;

impl Schedule {
    pub(crate) fn is_valid(&self) -> bool {
        self.days <= ALL_DAYS
            && [self.start, self.stop].into_iter().flatten().all(|m| m < MINUTES_PER_DAY)
    }

    /// The schedule's actions in `(from, to]`, oldest first.
    fn actions(&self, from: NaiveDateTime, to: NaiveDateTime) -> Vec<(NaiveDateTime, Action)> {
        let mut actions = Vec::new();
        if !self.enabled || from >= to {
            return actions;
        }
        // A day before `from`, for a stop that falls after midnight.
        let mut day = from.date() - TimeDelta::days(1);
        while day <= to.date() {
            if self.days & (1 << day.weekday().num_days_from_monday()) != 0 {
                let at = |minutes: u16, next_day: bool| {
                    let time = NaiveTime::MIN + TimeDelta::minutes(minutes.into());
                    day.and_time(time) + TimeDelta::days(next_day.into())
                };
                if let Some(start) = self.start {
                    actions.push((at(start, false), Action::Start));
                }
                if let Some(stop) = self.stop {
                    let next_day = self.start.is_some_and(|start| stop <= start);
                    actions.push((at(stop, next_day), Action::Stop));
                }
            }
            day += TimeDelta::days(1);
        }
        actions.retain(|(at, _)| *at > from && *at <= to);
        actions.sort_by_key(|(at, _)| *at);
        actions
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Start,
    Stop,
}

const TICK: Duration = Duration::from_secs(15);
/// Missed actions older than this, for example while the computer slept, do not run.
const MAX_CATCH_UP: TimeDelta = TimeDelta::days(1);

/// Runs queue schedules until the engine shuts down.
pub(crate) async fn run_scheduler(engine: Engine) {
    let mut last = Local::now().naive_local();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = engine.closed() => return,
        }
        let now = Local::now().naive_local();
        // Local times repeat when the clock goes back, as when daylight saving time ends. Waiting
        // until the clock passes `last` again keeps actions in the repeated hour from running
        // twice; a larger jump back means the clock was set and starts over.
        if now < last && last - now <= TimeDelta::hours(2) {
            continue;
        }
        let from = last.max(now - MAX_CATCH_UP).min(now);
        last = now;
        let queues = match engine.queues() {
            Ok(queues) => queues,
            Err(e) => {
                tracing::warn!("scheduler could not read queues: {e}");
                continue;
            }
        };
        for queue in queues {
            for (_, action) in queue.schedule.actions(from, now) {
                tracing::info!(queue = %queue.id, ?action, "scheduled queue action");
                let result = match action {
                    Action::Start => engine.start_queue(queue.id),
                    Action::Stop => engine.stop_queue(queue.id),
                };
                if let Err(e) = result {
                    tracing::warn!(queue = %queue.id, "scheduled {action:?} failed: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;

    /// 2026-10-19 is a Monday.
    fn at(day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, day).unwrap().and_hms_opt(hour, minute, 0).unwrap()
    }

    fn schedule(start: Option<u16>, stop: Option<u16>, days: u8) -> Schedule {
        Schedule { enabled: true, start, stop, days }
    }

    #[test]
    fn runs_actions_inside_the_window_only() {
        let s = schedule(Some(2 * 60), Some(6 * 60), ALL_DAYS);
        assert_eq!(s.actions(at(19, 1, 0), at(19, 2, 0)), [(at(19, 2, 0), Action::Start)]);
        assert_eq!(s.actions(at(19, 2, 0), at(19, 3, 0)), []);
        assert_eq!(
            s.actions(at(19, 1, 0), at(19, 7, 0)),
            [(at(19, 2, 0), Action::Start), (at(19, 6, 0), Action::Stop)]
        );
    }

    #[test]
    fn a_stop_before_the_start_is_on_the_next_day() {
        // Weekday nights: Friday 23:00 to Saturday 07:00 is included.
        let s = schedule(Some(23 * 60), Some(7 * 60), 0b001_1111);
        assert_eq!(s.actions(at(24, 6, 0), at(24, 8, 0)), [(at(24, 7, 0), Action::Stop)]);
        // But no stop on Monday morning, after a Sunday without a start.
        assert_eq!(s.actions(at(19, 6, 0), at(19, 8, 0)), []);
    }

    #[test]
    fn skips_unselected_days_and_disabled_schedules() {
        let s = schedule(Some(9 * 60), None, 1 << 2); // Wednesdays
        assert_eq!(s.actions(at(19, 0, 0), at(20, 23, 0)), []);
        assert_eq!(s.actions(at(21, 0, 0), at(21, 23, 0)), [(at(21, 9, 0), Action::Start)]);
        let disabled = Schedule { enabled: false, ..s };
        assert_eq!(disabled.actions(at(21, 0, 0), at(21, 23, 0)), []);
    }
}

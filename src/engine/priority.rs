use std::str::FromStr;
use std::sync::{Arc, OnceLock};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const HIGH_LABEL: &str = "priority:high";
const LOW_LABEL: &str = "priority:low";
const PREFIX: &str = "priority:";

/// Dispatch priority read from the `priority:high` and `priority:low` labels
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
}

impl Priority {
    /// The highest label wins when a task carries both
    pub fn from_labels(labels: &[String]) -> Self {
        let mut found = Self::Normal;
        for label in labels {
            match label.as_str() {
                HIGH_LABEL => return Self::High,
                LOW_LABEL => found = Self::Low,
                _ => {}
            }
        }
        found
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
        }
    }

    fn label(self) -> Option<&'static str> {
        match self {
            Self::Low => Some(LOW_LABEL),
            Self::Normal => None,
            Self::High => Some(HIGH_LABEL),
        }
    }

    /// `labels` with every priority label replaced by the one for `self`
    pub fn apply_to(self, labels: &[String]) -> Vec<String> {
        let mut out: Vec<String> = labels
            .iter()
            .filter(|l| !is_priority_label(l))
            .cloned()
            .collect();
        if let Some(label) = self.label() {
            out.push(label.to_string());
        }
        out
    }
}

impl FromStr for Priority {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "high" => Ok(Self::High),
            "normal" => Ok(Self::Normal),
            "low" => Ok(Self::Low),
            other => anyhow::bail!("unknown priority '{other}', use high, normal or low"),
        }
    }
}

pub fn is_priority_label(label: &str) -> bool {
    label.starts_with(PREFIX)
}

/// Highest priority first, arrival order kept inside a priority
pub fn sort_by_priority<T>(tasks: &mut [T], labels_of: impl Fn(&T) -> &[String]) {
    tasks.sort_by_key(|t| std::cmp::Reverse(Priority::from_labels(labels_of(t))));
}

/// The one slot a `priority:high` task may take beyond `max_parallel`
fn extra_slot() -> &'static Arc<Semaphore> {
    static SLOT: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SLOT.get_or_init(|| Arc::new(Semaphore::new(1)))
}

/// A regular slot, or for a high priority task the extra slot when every regular slot is busy
pub fn acquire_dispatch_slot(
    semaphore: &Arc<Semaphore>,
    priority: Priority,
) -> Option<OwnedSemaphorePermit> {
    acquire_with_extra(semaphore, extra_slot(), priority)
}

fn acquire_with_extra(
    semaphore: &Arc<Semaphore>,
    extra: &Arc<Semaphore>,
    priority: Priority,
) -> Option<OwnedSemaphorePermit> {
    if let Ok(permit) = Arc::clone(semaphore).try_acquire_owned() {
        return Some(permit);
    }

    if priority == Priority::High {
        return Arc::clone(extra).try_acquire_owned().ok();
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn reads_priority_from_labels() {
        assert_eq!(Priority::from_labels(&labels(&["bug"])), Priority::Normal);
        assert_eq!(
            Priority::from_labels(&labels(&["priority:low"])),
            Priority::Low
        );
        assert_eq!(
            Priority::from_labels(&labels(&["priority:low", "priority:high"])),
            Priority::High
        );
    }

    #[test]
    fn high_task_filed_later_dispatches_before_normal_task() {
        let mut tasks = vec![
            ("normal-first", labels(&["bug"])),
            ("low", labels(&["priority:low"])),
            ("high-later", labels(&["priority:high"])),
            ("normal-second", labels(&[])),
        ];
        sort_by_priority(&mut tasks, |t| &t.1);
        let order: Vec<_> = tasks.iter().map(|t| t.0).collect();
        assert_eq!(
            order,
            ["high-later", "normal-first", "normal-second", "low"]
        );
    }

    #[test]
    fn high_task_takes_the_extra_slot_and_a_second_one_waits() {
        let semaphore = Arc::new(Semaphore::new(1));
        let extra = Arc::new(Semaphore::new(1));
        let _busy = acquire_with_extra(&semaphore, &extra, Priority::Normal).unwrap();

        assert!(acquire_with_extra(&semaphore, &extra, Priority::Normal).is_none());
        assert!(acquire_with_extra(&semaphore, &extra, Priority::Low).is_none());

        let first_high = acquire_with_extra(&semaphore, &extra, Priority::High);
        assert!(first_high.is_some());
        assert!(acquire_with_extra(&semaphore, &extra, Priority::High).is_none());

        drop(first_high);
        assert!(acquire_with_extra(&semaphore, &extra, Priority::High).is_some());
    }

    #[test]
    fn high_task_prefers_a_regular_slot() {
        let semaphore = Arc::new(Semaphore::new(1));
        let extra = Arc::new(Semaphore::new(1));
        let _permit = acquire_with_extra(&semaphore, &extra, Priority::High).unwrap();
        assert_eq!(extra.available_permits(), 1);
    }

    #[test]
    fn apply_replaces_existing_priority_labels() {
        let current = labels(&["bug", "priority:low"]);
        assert_eq!(
            Priority::High.apply_to(&current),
            labels(&["bug", "priority:high"])
        );
        assert_eq!(Priority::Normal.apply_to(&current), labels(&["bug"]));
    }

    #[test]
    fn parses_priority_names() {
        assert_eq!("High".parse::<Priority>().unwrap(), Priority::High);
        assert!("urgent".parse::<Priority>().is_err());
    }
}

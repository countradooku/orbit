//! Automations and their run history as the store keeps them: rebuilt from
//! the meta log, written back when it is compacted.

use std::collections::{HashMap, VecDeque};

use orbit_proto::*;

/// Runs kept per automation, in memory and across compaction.
pub const KEEP_RUNS: usize = 100;
/// Runs per automation in a client snapshot.
pub const SNAPSHOT_RUNS: usize = 20;

#[derive(Default)]
pub struct Records {
    automations: HashMap<AutomationId, Automation>,
    /// Oldest first.
    runs: HashMap<AutomationId, VecDeque<AutomationRun>>,
}

impl Records {
    pub fn get(&self, id: AutomationId) -> Option<&Automation> {
        self.automations.get(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Automation> {
        self.automations.values()
    }

    pub fn len(&self) -> usize {
        self.automations.len() + self.runs.values().map(VecDeque::len).sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.automations.is_empty()
    }

    /// An automation's runs, newest first.
    pub fn runs(&self, id: AutomationId) -> impl Iterator<Item = &AutomationRun> {
        self.runs.get(&id).into_iter().flat_map(|runs| runs.iter().rev())
    }

    pub fn run(&self, automation: AutomationId, run: RunId) -> Option<&AutomationRun> {
        self.runs.get(&automation)?.iter().rev().find(|r| r.id == run)
    }

    /// Every run that is queued, checking or running.
    pub fn active_runs(&self) -> Vec<AutomationRun> {
        let mut runs: Vec<AutomationRun> =
            self.runs.values().flat_map(|r| r.iter().filter(|r| r.status.is_active()).cloned()).collect();
        runs.sort_by_key(|r| r.id);
        runs
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::AutomationUpserted(automation) => {
                observe_raw_id(automation.id.0);
                self.automations.insert(automation.id, *automation);
            }
            Event::AutomationRemoved(id) => {
                self.automations.remove(&id);
                self.runs.remove(&id);
            }
            Event::AutomationRun(run) => {
                if !self.automations.contains_key(&run.automation) {
                    return;
                }
                observe_raw_id(run.id.0);
                let runs = self.runs.entry(run.automation).or_default();
                match runs.iter_mut().rev().find(|r| r.id == run.id) {
                    Some(slot) => *slot = *run,
                    None => {
                        runs.push_back(*run);
                        while runs.len() > KEEP_RUNS {
                            runs.pop_front();
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// One record per automation and kept run, for a compacted log.
    pub fn records(&self, seq: u64) -> Vec<Envelope> {
        let mut out = Vec::with_capacity(self.len());
        for automation in self.automations.values() {
            out.push(Envelope { seq, event: Event::AutomationUpserted(Box::new(automation.clone())) });
        }
        for runs in self.runs.values() {
            for run in runs {
                out.push(Envelope { seq, event: Event::AutomationRun(Box::new(run.clone())) });
            }
        }
        out
    }

    /// (automations by creation, their newest runs newest first).
    pub fn snapshot(&self) -> (Vec<Automation>, Vec<AutomationRun>) {
        let mut automations: Vec<Automation> = self.automations.values().cloned().collect();
        automations.sort_by_key(|a| a.id);
        let runs = automations.iter().flat_map(|a| self.runs(a.id).take(SNAPSHOT_RUNS).cloned()).collect();
        (automations, runs)
    }
}

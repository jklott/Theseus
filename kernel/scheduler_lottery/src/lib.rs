//! This crate picks the next task in lottery fashion.
//! Each time a random task is picked from the queue.
//! The chance of a task being picked is proportional to its tickets.
//! Each time a task is picked, it is removed from the queue.
//! The task is then added back to the queue with its tickets reduced by 1.
//! This ensures that a task with more tickets is more likely to be picked.

#![no_std]

extern crate alloc;

use alloc::{boxed::Box, collections::VecDeque, vec::Vec};

use task::TaskRef;

pub struct Scheduler {
    idle_task: TaskRef,
    queue: VecDeque<TaskRef>,
}

impl Scheduler {
    pub const fn new(idle_task: TaskRef) -> Self {
        Self {
            idle_task,
            queue: VecDeque::new(),
        }
    }
}

impl task::scheduler::Scheduler for Scheduler {
    fn next(&mut self) -> TaskRef {
        let mut rng = rand::thread_rng();
        let mut tickets = 0;
        for task in self.queue.iter() {
            tickets += task.tickets();
        }

        let mut ticket = rng.gen_range(0..tickets);
        for task in self.queue.iter() {
            ticket -= task.tickets();
            if ticket < 0 {
                return task.clone();
            }
        }

        self.idle_task.clone()
    }

    fn busyness(&self) -> usize {
        self.queue.len()
    }

    fn add(&mut self, task: TaskRef) {
        self.queue.push_back(task);
    }

    fn remove(&mut self, task: &TaskRef) -> bool {
        let mut task_index = None;
        for (i, t) in self.queue.iter().enumerate() {
            if t == task {
                task_index = Some(i);
                break;
            }
        }

        if let Some(task_index) = task_index {
            self.queue.remove(task_index);
            true
        } else {
            false
        }
    }

    fn as_priority_scheduler(&mut self) -> Option<&mut dyn task::scheduler::PriorityScheduler> {
        None
    }

    fn drain(&mut self) -> Box<dyn Iterator<Item = TaskRef> + '_> {
        Box::new(self.queue.drain(..))
    }

    fn tasks(&self) -> Vec<TaskRef> {
        self.queue.clone().into()
    }
}

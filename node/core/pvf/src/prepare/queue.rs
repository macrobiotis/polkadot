// Copyright 2021 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

use super::{
	pool::{self, Worker},
	worker,
};
use crate::{artifacts::ArtifactId, priority, Priority, Pvf};
use futures::{
	Future, FutureExt, SinkExt,
	channel::{mpsc, oneshot},
	future::BoxFuture,
	stream::{FuturesOrdered, StreamExt as _},
};
use std::{
	collections::{HashMap, HashSet, VecDeque},
	iter, mem,
	task::Poll,
};
use async_std::path::PathBuf;

pub enum ToQueue {
	Enqueue { priority: Priority, pvf: Pvf },
}

pub enum FromQueue {
	Prepared(ArtifactId),
}

#[derive(Default)]
struct Limits {
	/// The number of workers either live or just spawned.
	spawned_num: usize,

	/// The maximum number of workers this pool can ever host. This is expected to be a small
	/// number, e.g. within a dozen.
	hard_capacity: usize,

	/// The number of workers we want aim to have. If there is a critical job and we are already
	/// at `soft_capacity`, we are allowed to grow up to `hard_capacity`. Thus this should be equal
	/// or smaller than `hard_capacity`.
	soft_capacity: usize,
}

impl Limits {
	/// Returns `true` if the queue is allowed to request one more worker.
	fn can_afford_one_more(&self, critical: bool) -> bool {
		let cap = if critical {
			self.hard_capacity
		} else {
			self.soft_capacity
		};
		self.spawned_num < cap
	}

	/// Offer the worker back to the pool. The passed worker ID must be considered unusable unless
	/// it wasn't taken by the pool, in which case it will be returned as `Some`.
	fn should_cull(&mut self) -> bool {
		self.spawned_num > self.soft_capacity
	}
}

struct Job {
	/// The artifact ID which is being prepared in the context of this job. Fixed throughout the
	/// execution of the job.
	artifact_id: ArtifactId,

	/// The priority of this job. Can be bumped.
	priority: Priority,

	pvf: Pvf,
}

/// TODO:
/// This structure is prone to starving, however, we don't care that much since we expect there is
/// going to be a limited number of critical jobs and we don't really care if background starve.
#[derive(Default)]
struct Unscheduled {
	background: VecDeque<Pvf>,
	normal: VecDeque<Pvf>,
	critical: VecDeque<Pvf>,
}

impl Unscheduled {
	fn add(&mut self, prio: Priority, pvf: Pvf) {
		match prio {
			Priority::Background => self.background.push_back(pvf),
			Priority::Normal => self.normal.push_back(pvf),
			Priority::Critical => self.critical.push_back(pvf),
		}
	}

	fn next(&mut self) -> Option<(Priority, Pvf)> {
		let mut check = |prio: Priority| {
			let q = match prio {
				Priority::Background => &mut self.background,
				Priority::Normal => &mut self.normal,
				Priority::Critical => &mut self.critical,
			};

			q.pop_front().map(|pvf| (prio, pvf))
		};
		None.or_else(|| check(Priority::Critical))
			.or_else(|| check(Priority::Normal))
			.or_else(|| check(Priority::Background))
	}
}

struct Queue {
	to_queue_rx: mpsc::Receiver<ToQueue>,
	from_queue_tx: mpsc::UnboundedSender<FromQueue>,

	to_pool_tx: mpsc::Sender<pool::ToPool>,
	from_pool_rx: mpsc::UnboundedReceiver<pool::FromPool>,

	cache_path: PathBuf,
	limits: Limits,

	// TODO:
	// None means that the artifact is enqueued but is not scheduled. Some means that the worker
	// is working on it.
	assignments: HashMap<ArtifactId, Option<Worker>>,

	jobs: slotmap::SecondaryMap<Worker, Job>,

	/// The set of workers that were spawned but do not have any work to do.
	idle: HashSet<Worker>,

	/// The jobs that are not yet scheduled. These are waiting until the next `poll` where they are
	/// processed all at once.
	unscheduled: Unscheduled,
}

/// A fatal error that warrants stopping the queue.
struct Fatal;

impl Queue {
	fn new(
		soft_capacity: usize,
		hard_capacity: usize,
		cache_path: PathBuf,
		to_queue_rx: mpsc::Receiver<ToQueue>,
		from_queue_tx: mpsc::UnboundedSender<FromQueue>,
		to_pool_tx: mpsc::Sender<pool::ToPool>,
		from_pool_rx: mpsc::UnboundedReceiver<pool::FromPool>,
	) -> Self {
		Self {
			limits: Limits {
				spawned_num: 0,
				soft_capacity,
				hard_capacity,
			},
			assignments: HashMap::new(),
			unscheduled: Unscheduled::default(),
			cache_path,
			to_queue_rx,
			from_queue_tx,
			to_pool_tx,
			from_pool_rx,
			idle: HashSet::new(),
			jobs: slotmap::SecondaryMap::new(),
		}
	}

	async fn run(mut self) {
		macro_rules! break_if_fatal {
			($expr:expr) => {
				if let Err(Fatal) = $expr {
					break;
					}
			};
		}

		loop {
			futures::select! {
				ToQueue::Enqueue { pvf, priority } = self.to_queue_rx.select_next_some() =>
					break_if_fatal!(enqueue(&mut self, priority, pvf).await),
				from_pool = self.from_pool_rx.select_next_some() =>
					break_if_fatal!(handle_from_pool(&mut self, from_pool).await),
			}
		}
	}
}

async fn enqueue(queue: &mut Queue, prio: Priority, pvf: Pvf) -> Result<(), Fatal> {
	if let Some(&worker) = queue.assignments.get(&pvf.to_artifact_id()) {
		// Preparation is already under way. Bump the priority if needed.
		let job = &mut queue.jobs[worker];
		if job.priority.is_background() && !prio.is_background() {
			send_pool(&mut queue.to_pool_tx, pool::ToPool::BumpPriority(worker)).await?;
		}
		job.priority = prio;
		return Ok(());
	}

	if let Some(available) = reserve_idle_worker(queue) {
		// TODO: Explain, why this should be fair, i.e. that the work won't be handled out of order.
		assign(queue, available, prio, pvf).await?;
	} else {
		spawn_extra_worker(queue, prio.is_critical()).await?;
		queue.unscheduled.add(prio, pvf);
	}

	Ok(())
}

fn reserve_idle_worker(queue: &mut Queue) -> Option<Worker> {
	if let Some(&free) = queue.idle.iter().next() {
		queue.idle.remove(&free);
		Some(free)
	} else {
		None
	}
}

async fn handle_from_pool(queue: &mut Queue, from_pool: pool::FromPool) -> Result<(), Fatal> {
	use pool::FromPool::*;
	match from_pool {
		Spawned(worker) => handle_worker_spawned(queue, worker).await?,
		Concluded(worker) => handle_worker_concluded(queue, worker).await?,
		Rip(worker) => handle_worker_rip(queue, worker).await?,
	}
	Ok(())
}

async fn handle_worker_spawned(queue: &mut Queue, worker: Worker) -> Result<(), Fatal> {
	if let Some((prio, pvf)) = queue.unscheduled.next() {
		assign(queue, worker, prio, pvf).await?;
	} else {
		queue.idle.insert(worker);
	}
	Ok(())
}

async fn handle_worker_concluded(queue: &mut Queue, worker: Worker) -> Result<(), Fatal> {
	let job = queue
		.jobs
		.remove(worker)
		.take()
		.expect("the worker was assigned so it should have had job; qed");

	let _ = queue.assignments.remove(&job.artifact_id);
	let artifact_id = job.artifact_id;

	if queue.limits.should_cull() {
		// We no longer need services of this worker. Kill it.
		send_pool(&mut queue.to_pool_tx, pool::ToPool::Kill(worker)).await?;
	} else {
		// see if there are more work available and schedule it.
		if let Some((prio, pvf)) = queue.unscheduled.next() {
			assign(queue, worker, prio, pvf).await?;
		} else {
			queue.idle.insert(worker);
		}

		reply(&mut queue.from_queue_tx, FromQueue::Prepared(artifact_id))?;
	}

	Ok(())
}

async fn handle_worker_rip(queue: &mut Queue, worker: Worker) -> Result<(), Fatal> {
	queue.limits.spawned_num -= 1;
	queue.idle.remove(&worker);

	if let Some(Job {
		artifact_id,
		priority,
		pvf,
	}) = queue.jobs.remove(worker)
	{
		queue.assignments.remove(&artifact_id);
		queue.unscheduled.add(priority, pvf);
	}

	// Spawn another worker to replace the ripped one. That unconditionally is not critical
	// even though the job might have been, just to not accidentally fill up the whole pool.
	spawn_extra_worker(queue, false).await?;

	Ok(())
}

async fn spawn_extra_worker(queue: &mut Queue, critical: bool) -> Result<(), Fatal> {
	if queue.limits.can_afford_one_more(critical) {
		queue.limits.spawned_num += 1;
		send_pool(&mut queue.to_pool_tx, pool::ToPool::Spawn).await?;
	}

	Ok(())
}

async fn assign(queue: &mut Queue, worker: Worker, prio: Priority, pvf: Pvf) -> Result<(), Fatal> {
	let artifact_id = pvf.to_artifact_id();
	let artifact_path = artifact_id.path(&queue.cache_path);

	queue.assignments.insert(artifact_id.clone(), Some(worker));
	queue.jobs.insert(
		worker,
		Job {
			artifact_id,
			priority: prio,
			pvf: pvf.clone(),
		},
	);

	send_pool(
		&mut queue.to_pool_tx,
		pool::ToPool::StartWork {
			worker,
			code: pvf.code,
			artifact_path,
			background_priority: prio.is_background(),
		},
	)
	.await?;

	Ok(())
}

fn reply(from_queue_tx: &mut mpsc::UnboundedSender<FromQueue>, m: FromQueue) -> Result<(), Fatal> {
	from_queue_tx.unbounded_send(m).map_err(|_| {
		// The host has hung up and thus it's fatal and we should shutdown ourselves.
		Fatal
	})
}

async fn send_pool(
	to_pool_tx: &mut mpsc::Sender<pool::ToPool>,
	m: pool::ToPool,
) -> Result<(), Fatal> {
	to_pool_tx.send(m).await.map_err(|_| {
		// The pool has hung up and thus we are no longer are able to fulfill our duties. Shutdown.
		Fatal
	})
}

pub fn start(
	soft_capacity: usize,
	hard_capacity: usize,
	cache_path: PathBuf,
	to_pool_tx: mpsc::Sender<pool::ToPool>,
	from_pool_rx: mpsc::UnboundedReceiver<pool::FromPool>,
) -> (
	mpsc::Sender<ToQueue>,
	mpsc::UnboundedReceiver<FromQueue>,
	impl Future<Output = ()>,
) {
	let (to_queue_tx, to_queue_rx) = mpsc::channel(150);
	let (from_queue_tx, from_queue_rx) = mpsc::unbounded();

	let run = Queue::new(
		soft_capacity,
		hard_capacity,
		cache_path,
		to_queue_rx,
		from_queue_tx,
		to_pool_tx,
		from_pool_rx,
	)
	.run();

	(to_queue_tx, from_queue_rx, run)
}

#[cfg(test)]
mod tests {
	use slotmap::SlotMap;

    use super::*;

	// TODO: respects priority for unscheduled
	// TODO: bumps priority if needed
	// TODO: doesn't exceed the hard limit unless needed
	// TODO: immune to rips

	fn pvf(descriminator: u32) -> Pvf {
		let d_buf = descriminator.to_le_bytes();
		Pvf::from_code(&d_buf)
	}

	#[async_std::test]
	async fn bump_prio_on_urgency_change() {
		let tempdir = tempfile::tempdir().unwrap();

		let (to_pool_tx, mut to_pool_rx) = mpsc::channel(10);
		let (mut from_pool_tx, from_pool_rx) = mpsc::unbounded();

		let mut workers: SlotMap<Worker, ()> = SlotMap::with_key();

		let (mut to_queue_tx, from_queue_rx, run) =
			start(2, 2, tempdir.path().to_owned().into(), to_pool_tx, from_pool_rx);

		let mut event_loop = async_std::task::spawn(run);

		to_queue_tx
			.send(ToQueue::Enqueue {
				priority: Priority::Background,
				pvf: pvf(1),
			})
			.await;

		assert_eq!(
			to_pool_rx.select_next_some().await,
			pool::ToPool::Spawn
		);

		let w = workers.insert(());
		from_pool_tx.send(pool::FromPool::Spawned(w));

		to_queue_tx
			.send(ToQueue::Enqueue {
				priority: Priority::Normal,
				pvf: pvf(1),
			})
			.await;

		assert_eq!(
			to_pool_rx.select_next_some().await,
			pool::ToPool::BumpPriority(w),
		);
	}
}

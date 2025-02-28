use crate::{library::Library, Node};

use std::{
	collections::{hash_map::DefaultHasher, VecDeque},
	fmt,
	hash::{Hash, Hasher},
	mem,
	sync::Arc,
	time::Instant,
};

use sd_prisma::prisma::location;

use async_channel as chan;
use futures::stream::{self, StreamExt};
use futures_concurrency::stream::Merge;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::{
	spawn,
	task::{JoinError, JoinHandle},
};
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

mod error;
mod manager;
mod report;
mod worker;

pub use error::*;
pub use manager::*;
pub use report::*;
pub use worker::*;

pub type JobResult = Result<JobMetadata, JobError>;
pub type JobMetadata = Option<serde_json::Value>;

#[derive(Debug)]
pub struct JobIdentity {
	pub id: Uuid,
	pub name: &'static str,
	pub target_location: location::id::Type,
	pub status: JobStatus,
}

#[derive(Debug, Default)]
pub struct JobRunErrors(pub Vec<String>);

impl JobRunErrors {
	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}
}

impl<I: IntoIterator<Item = String>> From<I> for JobRunErrors {
	fn from(errors: I) -> Self {
		Self(errors.into_iter().collect())
	}
}

impl fmt::Display for JobRunErrors {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}", self.0.join("\n"))
	}
}

pub struct JobRunOutput {
	pub metadata: JobMetadata,
	pub errors: JobRunErrors,
	pub next_job: Option<Box<dyn DynJob>>,
}

pub trait JobRunMetadata:
	Default + Serialize + DeserializeOwned + Send + Sync + fmt::Debug
{
	fn update(&mut self, new_data: Self);
}

impl JobRunMetadata for () {
	fn update(&mut self, _new_data: Self) {}
}

#[async_trait::async_trait]
pub trait StatefulJob:
	Serialize + DeserializeOwned + Hash + fmt::Debug + Send + Sync + Sized + 'static
{
	type Data: Serialize + DeserializeOwned + Send + Sync + fmt::Debug;
	type Step: Serialize + DeserializeOwned + Send + Sync + fmt::Debug;
	type RunMetadata: JobRunMetadata;

	/// The name of the job is a unique human readable identifier for the job.
	const NAME: &'static str;
	const IS_BACKGROUND: bool = false;
	const IS_BATCHED: bool = false;

	/// initialize the steps for the job
	async fn init(
		&self,
		ctx: &WorkerContext,
		data: &mut Option<Self::Data>,
	) -> Result<JobInitOutput<Self::RunMetadata, Self::Step>, JobError>;

	/// The location id where this job will act upon
	fn target_location(&self) -> location::id::Type;

	/// is called for each step in the job. These steps are created in the `Self::init` method.
	async fn execute_step(
		&self,
		ctx: &WorkerContext,
		step: CurrentStep<'_, Self::Step>,
		data: &Self::Data,
		run_metadata: &Self::RunMetadata,
	) -> Result<JobStepOutput<Self::Step, Self::RunMetadata>, JobError>;

	/// is called after all steps have been executed
	async fn finalize(
		&self,
		ctx: &WorkerContext,
		data: &Option<Self::Data>,
		run_metadata: &Self::RunMetadata,
	) -> JobResult;

	fn hash(&self) -> u64 {
		let mut s = DefaultHasher::new();
		Self::NAME.hash(&mut s);
		<Self as Hash>::hash(self, &mut s);
		s.finish()
	}
}

#[async_trait::async_trait]
pub trait DynJob: Send + Sync {
	fn id(&self) -> Uuid;
	fn parent_id(&self) -> Option<Uuid>;
	fn report(&self) -> &Option<JobReport>;
	fn report_mut(&mut self) -> &mut Option<JobReport>;
	fn name(&self) -> &'static str;
	async fn run(
		&mut self,
		ctx: WorkerContext,
		commands_rx: chan::Receiver<WorkerCommand>,
	) -> Result<JobRunOutput, JobError>;
	fn hash(&self) -> u64;
	fn set_next_jobs(&mut self, next_jobs: VecDeque<Box<dyn DynJob>>);
	fn serialize_state(&self) -> Result<Vec<u8>, JobError>;
	async fn register_children(&mut self, library: &Library) -> Result<(), JobError>;
	async fn pause_children(&mut self, library: &Library) -> Result<(), JobError>;
	async fn cancel_children(&mut self, library: &Library) -> Result<(), JobError>;
}

pub struct JobBuilder<SJob: StatefulJob> {
	id: Uuid,
	init: SJob,
	report_builder: JobReportBuilder,
}

impl<SJob: StatefulJob> JobBuilder<SJob> {
	pub fn build(self) -> Box<Job<SJob>> {
		Box::new(Job::<SJob> {
			id: self.id,
			hash: <SJob as StatefulJob>::hash(&self.init),
			report: Some(self.report_builder.build()),
			state: Some(JobState {
				init: self.init,
				data: None,
				steps: VecDeque::new(),
				step_number: 0,
				run_metadata: Default::default(),
			}),
			next_jobs: VecDeque::new(),
		})
	}

	pub fn new(init: SJob) -> Self {
		let id = Uuid::new_v4();
		Self {
			id,
			init,
			report_builder: JobReportBuilder::new(id, SJob::NAME.to_string()),
		}
	}

	pub fn with_action(mut self, action: impl AsRef<str>) -> Self {
		self.report_builder = self.report_builder.with_action(action);
		self
	}

	pub fn with_parent_id(mut self, parent_id: Uuid) -> Self {
		self.report_builder = self.report_builder.with_parent_id(parent_id);
		self
	}

	pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
		self.report_builder = self.report_builder.with_metadata(metadata);
		self
	}
}

pub struct Job<SJob: StatefulJob> {
	id: Uuid,
	hash: u64,
	report: Option<JobReport>,
	state: Option<JobState<SJob>>,
	next_jobs: VecDeque<Box<dyn DynJob>>,
}

impl<SJob: StatefulJob> Job<SJob> {
	pub fn new(init: SJob) -> Box<Self> {
		JobBuilder::new(init).build()
	}

	pub fn queue_next<NextSJob>(mut self: Box<Self>, init: NextSJob) -> Box<Self>
	where
		NextSJob: StatefulJob + 'static,
	{
		let next_job_order = self.next_jobs.len() + 1;

		let mut child_job_builder = JobBuilder::new(init).with_parent_id(self.id);

		if let Some(parent_report) = self.report() {
			if let Some(parent_action) = &parent_report.action {
				child_job_builder =
					child_job_builder.with_action(format!("{parent_action}-{next_job_order}"));
			}
		}

		self.next_jobs.push_back(child_job_builder.build());

		self
	}

	// this function returns an ingestible job instance from a job report
	pub fn new_from_report(
		mut report: JobReport,
		next_jobs: Option<VecDeque<Box<dyn DynJob>>>,
	) -> Result<Box<dyn DynJob>, JobError> {
		let state = rmp_serde::from_slice::<JobState<SJob>>(
			&report
				.data
				.take()
				.ok_or_else(|| JobError::MissingJobDataState(report.id, report.name.clone()))?,
		)?;

		Ok(Box::new(Self {
			id: report.id,
			hash: <SJob as StatefulJob>::hash(&state.init),
			state: Some(state),
			report: Some(report),
			next_jobs: next_jobs.unwrap_or_default(),
		}))
	}

	pub async fn spawn(
		self,
		node: &Arc<Node>,
		library: &Arc<Library>,
	) -> Result<(), JobManagerError> {
		node.jobs
			.clone()
			.ingest(node, library, Box::new(self))
			.await
	}
}

#[derive(Serialize)]
pub struct JobState<Job: StatefulJob> {
	pub init: Job,
	pub data: Option<Job::Data>,
	pub steps: VecDeque<Job::Step>,
	pub step_number: usize,
	pub run_metadata: Job::RunMetadata,
}

impl<'de, Job: StatefulJob> Deserialize<'de> for JobState<Job> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		<JobStateRaw<Job, Job> as Deserialize<'de>>::deserialize::<D>(deserializer).map(|raw| {
			JobState {
				init: raw.init,
				data: raw.data,
				steps: raw.steps,
				step_number: raw.step_number,
				run_metadata: raw.run_metadata,
			}
		})
	}
}

/// This is a workaround for a serde bug.
/// Both these generics on this type should point to the same type.
///
/// https://github.com/serde-rs/serde/issues/2418
/// https://github.com/rust-lang/rust/issues/34979
#[derive(Serialize, Deserialize)]
struct JobStateRaw<Job, JobInit>
where
	Job: StatefulJob,
{
	pub init: JobInit,
	pub data: Option<Job::Data>,
	pub steps: VecDeque<Job::Step>,
	pub step_number: usize,
	pub run_metadata: Job::RunMetadata,
}

pub struct JobInitOutput<RunMetadata, Step> {
	run_metadata: RunMetadata,
	steps: VecDeque<Step>,
	errors: JobRunErrors,
}

impl<RunMetadata, Step> From<(RunMetadata, Vec<Step>)> for JobInitOutput<RunMetadata, Step> {
	fn from((run_metadata, steps): (RunMetadata, Vec<Step>)) -> Self {
		Self {
			run_metadata,
			steps: VecDeque::from(steps),
			errors: Default::default(),
		}
	}
}

impl<RunMetadata, Step> From<Vec<Step>> for JobInitOutput<RunMetadata, Step>
where
	RunMetadata: Default,
{
	fn from(steps: Vec<Step>) -> Self {
		Self {
			run_metadata: RunMetadata::default(),
			steps: VecDeque::from(steps),
			errors: Default::default(),
		}
	}
}

impl<RunMetadata, Step> From<(RunMetadata, Vec<Step>, JobRunErrors)>
	for JobInitOutput<RunMetadata, Step>
{
	fn from((run_metadata, steps, errors): (RunMetadata, Vec<Step>, JobRunErrors)) -> Self {
		Self {
			run_metadata,
			steps: VecDeque::from(steps),
			errors,
		}
	}
}

pub struct CurrentStep<'step, Step> {
	pub step: &'step Step,
	pub step_number: usize,
}

pub struct JobStepOutput<Step, RunMetadata> {
	maybe_more_steps: Option<Vec<Step>>,
	maybe_more_metadata: Option<RunMetadata>,
	errors: JobRunErrors,
}

impl<Step, RunMetadata: JobRunMetadata> From<Vec<Step>> for JobStepOutput<Step, RunMetadata> {
	fn from(more_steps: Vec<Step>) -> Self {
		Self {
			maybe_more_steps: Some(more_steps),
			maybe_more_metadata: None,
			errors: Default::default(),
		}
	}
}

impl<Step, RunMetadata: JobRunMetadata> From<RunMetadata> for JobStepOutput<Step, RunMetadata> {
	fn from(more_metadata: RunMetadata) -> Self {
		Self {
			maybe_more_steps: None,
			maybe_more_metadata: Some(more_metadata),
			errors: Default::default(),
		}
	}
}

impl<Step, RunMetadata: JobRunMetadata> From<JobRunErrors> for JobStepOutput<Step, RunMetadata> {
	fn from(errors: JobRunErrors) -> Self {
		Self {
			maybe_more_steps: None,
			maybe_more_metadata: None,
			errors,
		}
	}
}

impl<Step, RunMetadata: JobRunMetadata> From<(Vec<Step>, RunMetadata)>
	for JobStepOutput<Step, RunMetadata>
{
	fn from((more_steps, more_metadata): (Vec<Step>, RunMetadata)) -> Self {
		Self {
			maybe_more_steps: Some(more_steps),
			maybe_more_metadata: Some(more_metadata),
			errors: Default::default(),
		}
	}
}

impl<Step, RunMetadata: JobRunMetadata> From<(RunMetadata, JobRunErrors)>
	for JobStepOutput<Step, RunMetadata>
{
	fn from((more_metadata, errors): (RunMetadata, JobRunErrors)) -> Self {
		Self {
			maybe_more_steps: None,
			maybe_more_metadata: Some(more_metadata),
			errors,
		}
	}
}

impl<Step, RunMetadata: JobRunMetadata> From<(Vec<Step>, RunMetadata, JobRunErrors)>
	for JobStepOutput<Step, RunMetadata>
{
	fn from((more_steps, more_metadata, errors): (Vec<Step>, RunMetadata, JobRunErrors)) -> Self {
		Self {
			maybe_more_steps: Some(more_steps),
			maybe_more_metadata: Some(more_metadata),
			errors,
		}
	}
}

impl<Step, RunMetadata: JobRunMetadata> From<Option<()>> for JobStepOutput<Step, RunMetadata> {
	fn from(_: Option<()>) -> Self {
		Self {
			maybe_more_steps: None,
			maybe_more_metadata: None,
			errors: Vec::new().into(),
		}
	}
}

#[async_trait::async_trait]
impl<SJob: StatefulJob> DynJob for Job<SJob> {
	fn id(&self) -> Uuid {
		// SAFETY: This method is using during queueing, so we still have a report
		self.report()
			.as_ref()
			.expect("This method is using during queueing, so we still have a report")
			.id
	}

	fn parent_id(&self) -> Option<Uuid> {
		self.report.as_ref().and_then(|r| r.parent_id)
	}

	fn report(&self) -> &Option<JobReport> {
		&self.report
	}

	fn report_mut(&mut self) -> &mut Option<JobReport> {
		&mut self.report
	}

	fn name(&self) -> &'static str {
		<SJob as StatefulJob>::NAME
	}

	async fn run(
		&mut self,
		ctx: WorkerContext,
		commands_rx: chan::Receiver<WorkerCommand>,
	) -> Result<JobRunOutput, JobError> {
		let job_name = self.name();
		let job_id = self.id;
		let mut errors = vec![];
		info!("Starting Job <id='{job_id}', name='{job_name}'>");

		let JobState {
			init,
			data,
			mut steps,
			mut step_number,
			mut run_metadata,
		} = self
			.state
			.take()
			.expect("critical error: missing job state");

		let target_location = init.target_location();

		let stateful_job = Arc::new(init);

		let ctx = Arc::new(ctx);

		let mut job_should_run = true;
		let job_init_time = Instant::now();

		// Checking if we have a brand new job, or if we are resuming an old one.
		let working_data = if let Some(data) = data {
			Some(data)
		} else {
			// Job init phase
			let init_time = Instant::now();
			let init_task = {
				let ctx = Arc::clone(&ctx);
				let stateful_job = Arc::clone(&stateful_job);
				spawn(async move {
					let mut new_data = None;
					let res = stateful_job.init(&ctx, &mut new_data).await;

					if let Ok(res) = res.as_ref() {
						if !<SJob as StatefulJob>::IS_BATCHED {
							ctx.progress(vec![JobReportUpdate::TaskCount(res.steps.len())]);
						}
					}

					(new_data, res)
				})
			};

			let InitPhaseOutput { maybe_data, output } = handle_init_phase::<SJob>(
				JobRunWorkTable {
					id: job_id,
					name: job_name,
					init_time,
					target_location,
				},
				Arc::clone(&ctx),
				init_task,
				commands_rx.clone(),
			)
			.await?;

			match output {
				Ok(JobInitOutput {
					run_metadata: new_run_metadata,
					steps: new_steps,
					errors: JobRunErrors(new_errors),
				}) => {
					steps = new_steps;
					errors.extend(new_errors);
					run_metadata.update(new_run_metadata);
				}
				Err(e @ JobError::EarlyFinish { .. }) => {
					info!("{e}");
					job_should_run = false;
				}
				Err(e) => return Err(e),
			}

			maybe_data
		};

		// Run the job until it's done or we get a command
		let data = if let Some(working_data) = working_data {
			let working_data_arc = Arc::new(working_data);

			// Job run phase
			while job_should_run && !steps.is_empty() {
				let steps_len = steps.len();

				let run_metadata_arc = Arc::new(run_metadata);
				let step = Arc::new(steps.pop_front().expect("just checked that we have steps"));

				let init_time = Instant::now();

				// JoinHandle<Result<JobStepOutput<SJob::Step, SJob::RunMetadata>, JobError>>
				let step_task = {
					// Need these bunch of Arcs to be able to move them into the async block of tokio::spawn
					let ctx = Arc::clone(&ctx);
					let run_metadata = Arc::clone(&run_metadata_arc);
					let working_data = Arc::clone(&working_data_arc);
					let step = Arc::clone(&step);
					let stateful_job = Arc::clone(&stateful_job);
					spawn(async move {
						stateful_job
							.execute_step(
								&ctx,
								CurrentStep {
									step: &step,
									step_number,
								},
								&working_data,
								&run_metadata,
							)
							.await
					})
				};

				let JobStepsPhaseOutput {
					steps: returned_steps,
					output,
				} = handle_single_step::<SJob>(
					JobRunWorkTable {
						id: job_id,
						name: job_name,
						init_time,
						target_location,
					},
					&job_init_time,
					(
						Arc::clone(&ctx),
						Arc::clone(&run_metadata_arc),
						Arc::clone(&working_data_arc),
						Arc::clone(&stateful_job),
					),
					JobStepDataWorkTable {
						step_number,
						steps,
						step,
						step_task,
					},
					commands_rx.clone(),
				)
				.await?;

				steps = returned_steps;

				run_metadata =
					Arc::try_unwrap(run_metadata_arc).expect("step already ran, no more refs");

				match output {
					Ok(JobStepOutput {
						maybe_more_steps,
						maybe_more_metadata,
						errors: JobRunErrors(new_errors),
					}) => {
						let mut events = vec![JobReportUpdate::CompletedTaskCount(step_number + 1)];

						if let Some(more_steps) = maybe_more_steps {
							events.push(JobReportUpdate::TaskCount(steps_len + more_steps.len()));

							steps.extend(more_steps);
						}

						if let Some(more_metadata) = maybe_more_metadata {
							run_metadata.update(more_metadata);
						}

						if !<SJob as StatefulJob>::IS_BATCHED {
							ctx.progress(events);
						}

						if !new_errors.is_empty() {
							warn!("Job<id='{job_id}', name='{job_name}'> had a step with errors");
							new_errors.iter().for_each(|err| {
								warn!("Job<id='{job_id}', name='{job_name}'> error: {:?}", err);
							});

							errors.extend(new_errors);
						}
					}
					Err(e @ JobError::EarlyFinish { .. }) => {
						info!("{e}");
						break;
					}
					Err(e) => return Err(e),
				}
				// remove the step from the queue
				step_number += 1;
			}

			debug!(
				"Total job run time {:?} Job <id='{job_id}', name='{job_name}'>",
				job_init_time.elapsed()
			);

			Some(Arc::try_unwrap(working_data_arc).expect("job already ran, no more refs"))
		} else {
			warn!("Tried to run a job without data Job <id='{job_id}', name='{job_name}'>");
			None
		};

		let metadata = stateful_job.finalize(&ctx, &data, &run_metadata).await?;

		let mut next_jobs = mem::take(&mut self.next_jobs);

		Ok(JobRunOutput {
			metadata,
			errors: errors.into(),
			next_job: next_jobs.pop_front().map(|mut next_job| {
				debug!(
					"Job<id='{job_id}', name='{job_name}'> requesting to spawn '{}' now that it's complete!",
					next_job.name()
				);
				next_job.set_next_jobs(next_jobs);

				next_job
			}),
		})
	}

	fn hash(&self) -> u64 {
		self.hash
	}

	fn set_next_jobs(&mut self, next_jobs: VecDeque<Box<dyn DynJob>>) {
		self.next_jobs = next_jobs;
	}

	fn serialize_state(&self) -> Result<Vec<u8>, JobError> {
		rmp_serde::to_vec_named(&self.state).map_err(Into::into)
	}

	async fn register_children(&mut self, library: &Library) -> Result<(), JobError> {
		for next_job in self.next_jobs.iter_mut() {
			if let Some(next_job_report) = next_job.report_mut() {
				if next_job_report.created_at.is_none() {
					next_job_report.create(library).await?
				}
			} else {
				return Err(JobError::MissingReport {
					id: next_job.id(),
					name: next_job.name().to_string(),
				});
			}
		}

		Ok(())
	}

	async fn pause_children(&mut self, library: &Library) -> Result<(), JobError> {
		for next_job in self.next_jobs.iter_mut() {
			let state = next_job.serialize_state()?;
			if let Some(next_job_report) = next_job.report_mut() {
				next_job_report.status = JobStatus::Paused;
				next_job_report.data = Some(state);
				next_job_report.update(library).await?;
			} else {
				return Err(JobError::MissingReport {
					id: next_job.id(),
					name: next_job.name().to_string(),
				});
			}
		}

		Ok(())
	}

	async fn cancel_children(&mut self, library: &Library) -> Result<(), JobError> {
		for next_job in self.next_jobs.iter_mut() {
			let state = next_job.serialize_state()?;
			if let Some(next_job_report) = next_job.report_mut() {
				next_job_report.status = JobStatus::Canceled;
				next_job_report.data = Some(state);
				next_job_report.update(library).await?;
			} else {
				return Err(JobError::MissingReport {
					id: next_job.id(),
					name: next_job.name().to_string(),
				});
			}
		}

		Ok(())
	}
}

struct InitPhaseOutput<SJob: StatefulJob> {
	maybe_data: Option<SJob::Data>,
	output: Result<JobInitOutput<SJob::RunMetadata, SJob::Step>, JobError>,
}

struct JobRunWorkTable {
	id: Uuid,
	name: &'static str,
	init_time: Instant,
	target_location: location::id::Type,
}

type InitTaskOutput<SJob> = (
	Option<<SJob as StatefulJob>::Data>,
	Result<
		JobInitOutput<<SJob as StatefulJob>::RunMetadata, <SJob as StatefulJob>::Step>,
		JobError,
	>,
);

#[inline]
async fn handle_init_phase<SJob: StatefulJob>(
	JobRunWorkTable {
		id,
		name,
		init_time,
		target_location,
	}: JobRunWorkTable,
	worker_ctx: Arc<WorkerContext>,
	init_task: JoinHandle<InitTaskOutput<SJob>>,
	mut commands_rx: chan::Receiver<WorkerCommand>,
) -> Result<InitPhaseOutput<SJob>, JobError> {
	enum StreamMessage<SJob: StatefulJob> {
		NewCommand(WorkerCommand),
		InitResult(Result<InitTaskOutput<SJob>, JoinError>),
	}

	let mut status = JobStatus::Running;

	let init_abort_handle = init_task.abort_handle();

	let mut msg_stream = (
		stream::once(init_task).map(StreamMessage::<SJob>::InitResult),
		commands_rx.clone().map(StreamMessage::<SJob>::NewCommand),
	)
		.merge();

	'messages: while let Some(msg) = msg_stream.next().await {
		match msg {
			StreamMessage::InitResult(Err(join_error)) => {
				error!(
					"Job <id='{id}', name='{name}'> \
							 failed to initialize due to an internal error: {join_error:#?}",
				);
				return Err(join_error.into());
			}
			StreamMessage::InitResult(Ok((maybe_data, output))) => {
				debug!(
					"Init phase took {:?} Job <id='{id}', name='{name}'>",
					init_time.elapsed()
				);

				return Ok(InitPhaseOutput { maybe_data, output });
			}
			StreamMessage::NewCommand(WorkerCommand::IdentifyYourself(tx)) => {
				if tx
					.send(JobIdentity {
						id,
						name,
						target_location,
						status,
					})
					.is_err()
				{
					warn!("Failed to send IdentifyYourself event reply");
				}
			}
			StreamMessage::NewCommand(WorkerCommand::Pause(when)) => {
				debug!(
					"Pausing Job at init phase <id='{id}', name='{name}'> took {:?}",
					when.elapsed()
				);

				// Notify the worker's work task that now we're paused
				worker_ctx.pause();

				status = JobStatus::Paused;

				// In case of a Pause command, we keep waiting for the next command
				let paused_time = Instant::now();
				while let Some(command) = commands_rx.next().await {
					match command {
						WorkerCommand::IdentifyYourself(tx) => {
							if tx
								.send(JobIdentity {
									id,
									name,
									target_location,
									status,
								})
								.is_err()
							{
								warn!("Failed to send IdentifyYourself event reply");
							}
						}
						WorkerCommand::Resume(when) => {
							debug!(
								"Resuming Job at init phase <id='{id}', name='{name}'> took {:?}",
								when.elapsed()
							);
							debug!(
								"Total paused time {:?} Job <id='{id}', name='{name}'>",
								paused_time.elapsed()
							);
							status = JobStatus::Running;

							continue 'messages;
						}
						// The job can also be shutdown or canceled while paused
						WorkerCommand::Shutdown(when, signal_tx) => {
							init_abort_handle.abort();

							debug!(
								"Shuting down Job at init phase <id='{id}', name='{name}'> \
									took {:?} after running for {:?}",
								when.elapsed(),
								init_time.elapsed(),
							);
							debug!("Total paused time {:?}", paused_time.elapsed());

							// Shutting down at init phase will abort the job
							return Err(JobError::Canceled(signal_tx));
						}
						WorkerCommand::Cancel(when, signal_tx) => {
							init_abort_handle.abort();
							debug!(
								"Canceling Job at init phase <id='{id}', name='{name}'> \
									took {:?} after running for {:?}",
								when.elapsed(),
								init_time.elapsed(),
							);
							debug!(
								"Total paused time {:?} Job <id='{id}', name='{name}'>",
								paused_time.elapsed()
							);
							return Err(JobError::Canceled(signal_tx));
						}
						WorkerCommand::Pause(_) => {
							// We continue paused lol
						}
						WorkerCommand::Timeout(elapsed, tx) => {
							error!(
								"Job <id='{id}', name='{name}'> \
								timed out at init phase after {elapsed:?} without updates"
							);
							tx.send(()).ok();
							return Err(JobError::Timeout(elapsed));
						}
					}
				}

				if commands_rx.is_closed() {
					error!(
						"Job <id='{id}', name='{name}'> \
						closed the command channel while paused"
					);
					return Err(JobError::Critical(
						"worker command channel closed while job was paused",
					));
				}
			}
			StreamMessage::NewCommand(WorkerCommand::Resume(_)) => {
				// We're already running so we just ignore this command
			}
			StreamMessage::NewCommand(WorkerCommand::Shutdown(when, signal_tx)) => {
				init_abort_handle.abort();

				debug!(
					"Shuting down at init phase Job <id='{id}', name='{name}'> took {:?} \
					after running for {:?}",
					when.elapsed(),
					init_time.elapsed(),
				);

				// Shutting down at init phase will abort the job
				return Err(JobError::Canceled(signal_tx));
			}
			StreamMessage::NewCommand(WorkerCommand::Cancel(when, signal_tx)) => {
				init_abort_handle.abort();

				debug!(
					"Canceling at init phase Job <id='{id}', name='{name}'> took {:?} \
					after running for {:?}",
					when.elapsed(),
					init_time.elapsed()
				);

				return Err(JobError::Canceled(signal_tx));
			}
			StreamMessage::NewCommand(WorkerCommand::Timeout(elapsed, tx)) => {
				error!(
					"Job <id='{id}', name='{name}'> \
					timed out at init phase after {elapsed:?} without updates"
				);
				tx.send(()).ok();
				return Err(JobError::Timeout(elapsed));
			}
		}
	}

	Err(JobError::Critical("unexpect job init end without result"))
}

type StepTaskOutput<SJob> = Result<
	JobStepOutput<<SJob as StatefulJob>::Step, <SJob as StatefulJob>::RunMetadata>,
	JobError,
>;

struct JobStepDataWorkTable<SJob: StatefulJob> {
	step_number: usize,
	steps: VecDeque<SJob::Step>,
	step: Arc<SJob::Step>,
	step_task: JoinHandle<StepTaskOutput<SJob>>,
}

struct JobStepsPhaseOutput<SJob: StatefulJob> {
	steps: VecDeque<SJob::Step>,
	output: StepTaskOutput<SJob>,
}

type StepArcs<SJob> = (
	Arc<WorkerContext>,
	Arc<<SJob as StatefulJob>::RunMetadata>,
	Arc<<SJob as StatefulJob>::Data>,
	Arc<SJob>,
);

#[inline]
async fn handle_single_step<SJob: StatefulJob>(
	JobRunWorkTable {
		id,
		name,
		init_time,
		target_location,
	}: JobRunWorkTable,
	job_init_time: &Instant,
	(worker_ctx, run_metadata, working_data, stateful_job): StepArcs<SJob>,
	JobStepDataWorkTable {
		step_number,
		mut steps,
		step,
		mut step_task,
	}: JobStepDataWorkTable<SJob>,
	mut commands_rx: chan::Receiver<WorkerCommand>,
) -> Result<JobStepsPhaseOutput<SJob>, JobError> {
	enum StreamMessage<SJob: StatefulJob> {
		NewCommand(WorkerCommand),
		StepResult(Result<StepTaskOutput<SJob>, JoinError>),
	}

	let mut status = JobStatus::Running;

	let mut msg_stream = (
		stream::once(&mut step_task).map(StreamMessage::<SJob>::StepResult),
		commands_rx.clone().map(StreamMessage::<SJob>::NewCommand),
	)
		.merge();

	'messages: while let Some(msg) = msg_stream.next().await {
		match msg {
			StreamMessage::StepResult(Err(join_error)) => {
				error!(
					"Job <id='{id}', name='{name}'> \
					failed to run step #{step_number} due to an internal error: {join_error:#?}",
				);
				return Err(join_error.into());
			}
			StreamMessage::StepResult(Ok(output)) => {
				trace!(
					"Step finished in {:?} Job <id='{id}', name='{name}'>",
					init_time.elapsed(),
				);

				return Ok(JobStepsPhaseOutput { steps, output });
			}
			StreamMessage::NewCommand(WorkerCommand::IdentifyYourself(tx)) => {
				if tx
					.send(JobIdentity {
						id,
						name,
						target_location,
						status,
					})
					.is_err()
				{
					warn!("Failed to send IdentifyYourself event reply");
				}
			}
			StreamMessage::NewCommand(WorkerCommand::Pause(when)) => {
				debug!(
					"Pausing Job <id='{id}', name='{name}'> took {:?}",
					when.elapsed()
				);

				worker_ctx.pause();

				status = JobStatus::Paused;

				// In case of a Pause command, we keep waiting for the next command
				let paused_time = Instant::now();
				while let Some(command) = commands_rx.next().await {
					match command {
						WorkerCommand::IdentifyYourself(tx) => {
							if tx
								.send(JobIdentity {
									id,
									name,
									target_location,
									status,
								})
								.is_err()
							{
								warn!("Failed to send IdentifyYourself event reply");
							}
						}
						WorkerCommand::Resume(when) => {
							debug!(
								"Resuming Job <id='{id}', name='{name}'> took {:?}",
								when.elapsed(),
							);
							debug!(
								"Total paused time {:?} Job <id='{id}', name='{name}'>",
								paused_time.elapsed(),
							);
							status = JobStatus::Running;

							continue 'messages;
						}
						// The job can also be shutdown or canceled while paused
						WorkerCommand::Shutdown(when, signal_tx) => {
							step_task.abort();
							let _ = step_task.await;

							debug!(
								"Shuting down Job <id='{id}', name='{name}'> took {:?} \
								after running for {:?}",
								when.elapsed(),
								job_init_time.elapsed(),
							);
							debug!(
								"Total paused time {:?} Job <id='{id}', name='{name}'>",
								paused_time.elapsed(),
							);

							// Taking back the last step, so it can run to completion later
							steps.push_front(
								Arc::try_unwrap(step).expect("step already ran, no more refs"),
							);

							return Err(JobError::Paused(
								rmp_serde::to_vec_named(&JobState::<SJob> {
									init: Arc::try_unwrap(stateful_job)
										.expect("handle abort already ran, no more refs"),
									data: Some(
										Arc::try_unwrap(working_data)
											.expect("handle abort already ran, no more refs"),
									),
									steps,
									step_number,
									run_metadata: Arc::try_unwrap(run_metadata)
										.expect("handle abort already ran, no more refs"),
								})?,
								signal_tx,
							));
						}
						WorkerCommand::Cancel(when, signal_tx) => {
							step_task.abort();
							let _ = step_task.await;
							debug!(
								"Canceling Job <id='{id}', name='{name}'> \
								took {:?} after running for {:?}",
								when.elapsed(),
								job_init_time.elapsed(),
							);
							debug!(
								"Total paused time {:?} Job <id='{id}', name='{name}'>",
								paused_time.elapsed(),
							);
							return Err(JobError::Canceled(signal_tx));
						}
						WorkerCommand::Pause(_) => {
							// We continue paused lol
						}

						WorkerCommand::Timeout(elapsed, tx) => {
							error!(
								"Job <id='{id}', name='{name}'> \
								timed out at step #{step_number} after {elapsed:?} without updates"
							);
							tx.send(()).ok();
							return Err(JobError::Timeout(elapsed));
						}
					}
				}

				if commands_rx.is_closed() {
					error!(
						"Job <id='{id}', name='{name}'> \
						closed the command channel while paused"
					);
					return Err(JobError::Critical(
						"worker command channel closed while job was paused",
					));
				}
			}
			StreamMessage::NewCommand(WorkerCommand::Resume(_)) => {
				// We're already running so we just ignore this command
			}
			StreamMessage::NewCommand(WorkerCommand::Shutdown(when, signal_tx)) => {
				step_task.abort();
				let _ = step_task.await;

				debug!(
					"Shuting down Job <id='{id}', name='{name}'> took {:?} \
					after running for {:?}",
					when.elapsed(),
					job_init_time.elapsed(),
				);

				// Taking back the last step, so it can run to completion later
				steps.push_front(
					Arc::try_unwrap(step).expect("handle abort already ran, no more refs"),
				);

				return Err(JobError::Paused(
					rmp_serde::to_vec_named(&JobState::<SJob> {
						init: Arc::try_unwrap(stateful_job)
							.expect("handle abort already ran, no more refs"),
						data: Some(
							Arc::try_unwrap(working_data)
								.expect("handle abort already ran, no more refs"),
						),
						steps,
						step_number,
						run_metadata: Arc::try_unwrap(run_metadata)
							.expect("step already ran, no more refs"),
					})?,
					signal_tx,
				));
			}
			StreamMessage::NewCommand(WorkerCommand::Cancel(when, signal_tx)) => {
				step_task.abort();
				let _ = step_task.await;
				debug!(
					"Canceling Job <id='{id}', name='{name}'> took {:?} \
										 after running for {:?}",
					when.elapsed(),
					job_init_time.elapsed(),
				);
				return Err(JobError::Canceled(signal_tx));
			}
			StreamMessage::NewCommand(WorkerCommand::Timeout(elapsed, tx)) => {
				error!(
					"Job <id='{id}', name='{name}'> \
					timed out at step #{step_number} after {elapsed:?} without updates"
				);
				tx.send(()).ok();
				return Err(JobError::Timeout(elapsed));
			}
		}
	}

	Err(JobError::Critical("unexpect job step end without result"))
}

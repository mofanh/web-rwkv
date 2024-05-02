use std::future::Future;

use anyhow::Result;
use web_rwkv_derive::Deref;

pub mod infer;
pub mod loader;
pub mod model;
pub mod softmax;
pub mod v4;
pub mod v5;
pub mod v6;

/// A [`Job`] to be executed on GPU.
pub trait Job: Sized + Send + 'static {
    type Info;
    type Input;
    type Output;

    /// Check if the input is compatible.
    fn check(&self, input: &Self::Input, info: &Self::Info) -> bool;
    /// Load the data from CPU to GPU.
    fn load(self, input: &Self::Input) -> Result<Self>;
    /// Submit the job to GPU and execute it immediately.
    fn submit(&mut self);
    /// Wait for the job to finish and read the data back.
    fn back(self) -> impl Future<Output = Result<Self::Output>> + Send;
}

pub trait JobBuilder<J: Job>: Send + 'static {
    type Info;

    /// Build a [`Job`] from the given info.
    /// This usually involves creating a list of GPU commands (but not actually execution).
    fn build(&self, info: Self::Info) -> impl Future<Output = Result<J>> + Send;
}

#[derive(Debug)]
pub struct Submission<I, O> {
    pub input: I,
    pub sender: tokio::sync::oneshot::Sender<(I, O)>,
}

pub trait JobInput: Send + 'static {
    /// One chunk of the whole input at a step.
    type Chunk: Send + 'static;

    /// Advance the input for a step.
    fn step(&mut self);
    /// The current step's chunk to feed into the job.
    fn chunk(&self) -> Self::Chunk;
}

#[derive(Debug, Clone, Deref)]
pub struct JobRuntime<I, O>(tokio::sync::mpsc::Sender<Submission<I, O>>);

#[allow(clippy::type_complexity)]
impl<I, O, T, F> JobRuntime<I, O>
where
    T: Send + 'static,
    F: Iterator<Item = T> + Send + 'static,
    I: JobInput,
    O: Send + 'static,
    for<'a> &'a I: IntoIterator<Item = T, IntoIter = F>,
{
    pub async fn new<J>(builder: impl JobBuilder<J, Info = T>) -> Self
    where
        J: Job<Info = T, Input = I::Chunk, Output = O>,
    {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let handle = tokio::spawn(Self::run(builder, receiver));
        tokio::spawn(async move {
            match handle.await {
                Ok(_) => {}
                Err(err) => log::error!("{}", err),
            }
        });
        Self(sender)
    }

    async fn run<J>(
        builder: impl JobBuilder<J, Info = T>,
        mut receiver: tokio::sync::mpsc::Receiver<Submission<I, O>>,
    ) -> Result<()>
    where
        J: Job<Info = T, Input = I::Chunk, Output = O>,
    {
        let mut predict: Option<J> = None;
        while let Some(Submission { input, sender }) = receiver.recv().await {
            let mut iter = (&input).into_iter();
            let Some(info) = iter.next() else {
                continue;
            };
            let next = iter.next();
            drop(iter);

            fn check<J: Job>(job: J, input: &J::Input, info: &J::Info) -> Option<J> {
                job.check(input, info).then_some(job)
            }

            let chunk = input.chunk();
            let mut job = match predict.take().and_then(|job| check(job, &chunk, &info)) {
                Some(job) => job,
                None => builder.build(info).await?,
            }
            .load(&chunk)?;

            async fn back<J: Job, I: JobInput>(
                job: J,
                mut input: I,
                sender: tokio::sync::oneshot::Sender<(I, J::Output)>,
            ) -> Result<()> {
                let output = job.back().await?;
                input.step();
                let _ = sender.send((input, output));
                Ok(())
            }

            job.submit();
            let handle = tokio::spawn(back(job, input, sender));

            predict = match next {
                Some(info) => Some(builder.build(info).await?),
                None => None,
            };
            handle.await??;
        }
        Ok(())
    }
}
//! Host-supplied results fed back into a suspended run:
//! [`NameLookupResult`], [`ExtFunctionResult`] and the [`FutureResults`]
//! batch that settles awaited calls.

use crate::{
    exceptions::MontyException,
    graph::{GraphError, MontyGraph, NodeId},
    object::{MontyObject, unstable::PushValue},
};

/// Result of a name lookup from the host.
///
/// When the VM encounters an unresolved name (or a lazy attribute on a
/// host-backed object), the host provides one of these:
/// - `Value(obj)`: The name resolves to this value (a plain name is cached in
///   its namespace slot; an attribute is re-consulted on every access).
/// - `Undefined`: The name does not exist — `NameError` for a plain name,
///   `AttributeError` for an attribute.
/// - `Error(exc)`: Resolving it raised on the host — the exception is raised
///   inside the sandbox, so `hasattr()` / `getattr()` defaults do not apply.
#[derive(Debug)]
pub enum NameLookupResult {
    /// The name resolves to this value.
    Value(MontyObject),
    /// The name is undefined — the VM raises `NameError` / `AttributeError`.
    Undefined,
    /// Resolving the name raised this exception on the host; the VM raises
    /// it where the lookup suspended, like a failed external call.
    Error(MontyException),
}

impl From<MontyObject> for NameLookupResult {
    fn from(value: MontyObject) -> Self {
        Self::Value(value)
    }
}

impl From<Option<MontyObject>> for NameLookupResult {
    /// `Some` resolves the name, `None` leaves it undefined.
    fn from(value: Option<MontyObject>) -> Self {
        value.map_or(Self::Undefined, Self::Value)
    }
}

impl From<MontyException> for NameLookupResult {
    fn from(exception: MontyException) -> Self {
        Self::Error(exception)
    }
}

/// Return value or exception from an external function.
#[derive(Debug)]
pub enum ExtFunctionResult {
    /// Continues execution with the return value from the external function.
    Return(MontyObject),
    /// Continues execution with the exception raised by the external function.
    Error(MontyException),
    /// Pending future — the external function is a coroutine.
    ///
    /// The `u32` is the `call_id` from the `FunctionCall` that created this
    /// snapshot. It is used to track the pending future so it can be resolved
    /// later via `ResolveFutures::resume()`.
    Future(u32),
    /// Raises `NameError`, or `ModuleNotFoundError` for an `import` call.
    NotFound(String),
}

impl From<MontyObject> for ExtFunctionResult {
    fn from(value: MontyObject) -> Self {
        Self::Return(value)
    }
}

impl From<MontyException> for ExtFunctionResult {
    fn from(exception: MontyException) -> Self {
        Self::Error(exception)
    }
}

/// Settled futures for `ResolveFutures::resume_batch`, in the order they settled.
///
/// The returned values share one arena, which resuming imports once: a node two
/// results reach is one sandbox object, and the import costs the arena rather
/// than the result count times each value. [`push`](Self::push) appends a
/// value's own nodes, so separate calls still get separate copies.
#[derive(Debug, Default)]
pub struct FutureResults {
    /// The arena holding every returned value.
    graph: MontyGraph,
    /// `(call_id, outcome)` pairs, in settling order.
    results: Vec<(u32, FutureOutcome)>,
}

impl FutureResults {
    /// No results.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends the result of call `call_id`.
    pub fn push(&mut self, call_id: u32, result: ExtFunctionResult) {
        let outcome = match result {
            ExtFunctionResult::Return(value) => FutureOutcome::Return(value.push_into(&mut self.graph)),
            ExtFunctionResult::Error(exc) => FutureOutcome::Error(exc),
            ExtFunctionResult::Future(id) => FutureOutcome::Future(id),
            ExtFunctionResult::NotFound(name) => FutureOutcome::NotFound(name),
        };
        self.results.push((call_id, outcome));
    }

    /// Number of results.
    #[must_use]
    pub fn len(&self) -> usize {
        self.results.len()
    }

    /// Whether there are no results.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.results.is_empty()
    }

    /// The call ids, in order; a call settled twice appears twice.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn call_ids(&self) -> impl ExactSizeIterator<Item = u32> {
        self.results.iter().map(|(call_id, _)| *call_id)
    }

    /// Checks every returned value's root is inside the arena; run on decoded messages.
    fn check_roots(&self) -> Result<(), GraphError> {
        self.results.iter().try_for_each(|(_, outcome)| match outcome {
            FutureOutcome::Return(root) => self.graph.check_root(*root),
            _ => Ok(()),
        })
    }
}

/// Results in settling order, each value pushed with its own nodes.
impl From<Vec<(u32, ExtFunctionResult)>> for FutureResults {
    fn from(results: Vec<(u32, ExtFunctionResult)>) -> Self {
        let mut batch = Self::new();
        for (call_id, result) in results {
            batch.push(call_id, result);
        }
        batch
    }
}

/// One settled call in a [`FutureResults`] batch: an [`ExtFunctionResult`]
/// whose returned value is a root of the batch's arena.
#[derive(Debug)]
pub enum FutureOutcome {
    /// The call returned the value rooted at this node.
    Return(NodeId),
    /// The call raised this exception.
    Error(MontyException),
    /// The call is still pending; the batch leaves it unresolved.
    Future(u32),
    /// Raises `NameError`.
    NotFound(String),
}

/// Takes the arena and `(call_id, outcome)` pairs of a batch without copying.
#[must_use]
pub fn into_future_results_parts(results: FutureResults) -> (MontyGraph, Vec<(u32, FutureOutcome)>) {
    (results.graph, results.results)
}

/// Builds a batch from graph storage, rejecting out-of-range value roots.
/// Results may share nodes; resume then imports a shared node once.
pub fn future_results_from_parts(
    graph: MontyGraph,
    results: Vec<(u32, FutureOutcome)>,
) -> Result<FutureResults, GraphError> {
    let results = FutureResults { graph, results };
    results.check_roots()?;
    Ok(results)
}

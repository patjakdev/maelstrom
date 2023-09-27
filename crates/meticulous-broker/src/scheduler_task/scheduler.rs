//! Central processing module for the broker. Receives and sends messages to and from clients and
//! workers.

use super::GetArtifact;
use meticulous_base::{
    proto::{BrokerToClient, BrokerToWorker, ClientToBroker, WorkerToBroker},
    BrokerStatistics, ClientExecutionId, ClientId, ExecutionDetails, ExecutionId, ExecutionResult,
    Sha256Digest, WorkerId,
};
use meticulous_util::{heap::{Heap, HeapDeps, HeapIndex}, OptionExt, BoolExt};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
};

/*              _     _ _
 *  _ __  _   _| |__ | (_) ___
 * | '_ \| | | | '_ \| | |/ __|
 * | |_) | |_| | |_) | | | (__
 * | .__/ \__,_|_.__/|_|_|\___|
 * |_|
 *  FIGLET: public
 */

/// The external dependencies for [Scheduler]. All of these methods must be asynchronous: they
/// must not block the current task or thread.
pub trait SchedulerDeps {
    type ClientSender;
    type WorkerSender;
    type WorkerArtifactFetcherSender;
    fn send_message_to_client(&mut self, sender: &mut Self::ClientSender, message: BrokerToClient);
    fn send_message_to_worker(&mut self, sender: &mut Self::WorkerSender, message: BrokerToWorker);
    fn send_message_to_worker_artifact_fetcher(
        &mut self,
        sender: &mut Self::WorkerArtifactFetcherSender,
        message: Option<PathBuf>,
    );
}

/// The required interface for the cache that is provided to the [Scheduler]. This mirrors the API
/// for [Cache]. We just keep them separate so that we can test the [Scheduler] more easily.
///
/// Unlike with [SchedulerDeps], all of these functions are immediate. The [Cache] is owned by the
/// [Scheduler] and the two live on the same task. So these methods sometimes return actual values
/// which can be handled immediately, unlike [SchedulerDeps].
pub trait SchedulerCache {
    fn get_artifact(&mut self, eid: ExecutionId, digest: Sha256Digest) -> GetArtifact;
    fn got_artifact(
        &mut self,
        digest: Sha256Digest,
        path: &Path,
        bytes_used: u64,
    ) -> Vec<ExecutionId>;
    fn decrement_refcount(&mut self, digest: Sha256Digest);
    fn client_disconnected(&mut self, cid: ClientId);
    fn get_artifact_for_worker(&mut self, digest: &Sha256Digest) -> Option<PathBuf>;
}

#[derive(Debug)]
pub enum Message<DepsT: SchedulerDeps> {
    ClientConnected(ClientId, DepsT::ClientSender),
    ClientDisconnected(ClientId),
    FromClient(ClientId, ClientToBroker),
    WorkerConnected(WorkerId, usize, DepsT::WorkerSender),
    WorkerDisconnected(WorkerId),
    FromWorker(WorkerId, WorkerToBroker),
    #[allow(dead_code)]
    GotArtifact(Sha256Digest, PathBuf, u64),
    #[allow(dead_code)]
    GetArtifactForWorker(Sha256Digest, DepsT::WorkerArtifactFetcherSender),
    #[allow(dead_code)]
    DecrementRefcount(Sha256Digest),
}

impl<CacheT: SchedulerCache, DepsT: SchedulerDeps> Scheduler<CacheT, DepsT> {
    pub fn new(cache: CacheT) -> Self {
        Scheduler {
            cache,
            clients: HashMap::default(),
            workers: WorkerMap(HashMap::default()),
            queued_requests: VecDeque::default(),
            worker_heap: Heap::default(),
        }
    }

    pub fn receive_message(&mut self, deps: &mut DepsT, msg: Message<DepsT>) {
        match msg {
            Message::ClientConnected(id, sender) => self.receive_client_connected(id, sender),
            Message::ClientDisconnected(id) => self.receive_client_disconnected(deps, id),
            Message::FromClient(cid, ClientToBroker::ExecutionRequest(ceid, details)) => {
                self.receive_client_execution_request(deps, cid, ceid, details)
            }
            Message::FromClient(cid, ClientToBroker::StatisticsRequest) => {
                self.receive_client_statistics_request(deps, cid)
            }
            Message::WorkerConnected(id, slots, sender) => {
                self.receive_worker_connected(deps, id, slots, sender)
            }
            Message::WorkerDisconnected(id) => self.receive_worker_disconnected(deps, id),
            Message::FromWorker(wid, WorkerToBroker(eid, result)) => {
                self.receive_worker_response(deps, wid, eid, result)
            }
            Message::GotArtifact(digest, path, bytes_used) => {
                self.receive_got_artifact(deps, digest, path, bytes_used)
            }
            Message::GetArtifactForWorker(digest, sender) => {
                self.receive_get_artifact_for_worker(deps, digest, sender)
            }
            Message::DecrementRefcount(digest) => self.receive_decrement_refcount(digest),
        }
    }
}

/*             _            _
 *  _ __  _ __(_)_   ____ _| |_ ___
 * | '_ \| '__| \ \ / / _` | __/ _ \
 * | |_) | |  | |\ V / (_| | ||  __/
 * | .__/|_|  |_| \_/ \__,_|\__\___|
 * |_|
 *  FIGLET: private
 */

struct Execution {
    details: ExecutionDetails,
    acquired_artifacts: HashSet<Sha256Digest>,
    missing_artifacts: HashSet<Sha256Digest>,
}

impl Execution {
    fn new(details: ExecutionDetails) -> Self {
        Execution {
            details,
            acquired_artifacts: HashSet::default(),
            missing_artifacts: HashSet::default(),
        }
    }
}

struct Client<DepsT: SchedulerDeps> {
    sender: DepsT::ClientSender,
    executions: HashMap<ClientExecutionId, Execution>,
}

impl<DepsT: SchedulerDeps> Client<DepsT> {
    fn new(sender: DepsT::ClientSender) -> Self {
        Client {
            sender,
            executions: HashMap::default(),
        }
    }
}

struct Worker<DepsT: SchedulerDeps> {
    slots: usize,
    pending: HashSet<ExecutionId>,
    heap_index: HeapIndex,
    sender: DepsT::WorkerSender,
}

impl<DepsT: SchedulerDeps> Worker<DepsT> {
    fn new(slots: usize, sender: DepsT::WorkerSender) -> Self {
        Worker {
            slots,
            sender,
            pending: HashSet::default(),
            heap_index: HeapIndex::default(),
        }
    }
}

struct WorkerMap<DepsT: SchedulerDeps>(HashMap<WorkerId, Worker<DepsT>>);

impl<DepsT: SchedulerDeps> HeapDeps for WorkerMap<DepsT> {
    type Element = WorkerId;

    fn is_element_less_than(&self, lhs_id: &WorkerId, rhs_id: &WorkerId) -> bool {
        let lhs_worker = self.0.get(lhs_id).unwrap();
        let rhs_worker = self.0.get(rhs_id).unwrap();
        let lhs = (lhs_worker.pending.len() * rhs_worker.slots, *lhs_id);
        let rhs = (rhs_worker.pending.len() * lhs_worker.slots, *rhs_id);
        lhs.cmp(&rhs) == std::cmp::Ordering::Less
    }

    fn update_index(&mut self, elem: &WorkerId, idx: HeapIndex) {
        self.0.get_mut(elem).unwrap().heap_index = idx;
    }
}

pub struct Scheduler<CacheT, DepsT: SchedulerDeps> {
    cache: CacheT,
    clients: HashMap<ClientId, Client<DepsT>>,
    workers: WorkerMap<DepsT>,
    queued_requests: VecDeque<ExecutionId>,
    worker_heap: Heap<WorkerMap<DepsT>>,
}

impl<CacheT: SchedulerCache, DepsT: SchedulerDeps> Scheduler<CacheT, DepsT> {
    fn possibly_start_executions(&mut self, deps: &mut DepsT) {
        while !self.queued_requests.is_empty() && !self.workers.0.is_empty() {
            let wid = self.worker_heap.peek().unwrap();
            let worker = self.workers.0.get_mut(wid).unwrap();

            if worker.pending.len() == 2 * worker.slots {
                break;
            }

            let eid = self.queued_requests.pop_front().unwrap();
            let details = &self
                .clients
                .get(&eid.0)
                .unwrap()
                .executions
                .get(&eid.1)
                .unwrap()
                .details;
            deps.send_message_to_worker(
                &mut worker.sender,
                BrokerToWorker::EnqueueExecution(eid, details.clone()),
            );

            worker.pending.insert(eid).assert_is_true();
            let heap_index = worker.heap_index;
            self.worker_heap.sift_down(&mut self.workers, heap_index);
        }
    }

    fn receive_client_connected(&mut self, id: ClientId, sender: DepsT::ClientSender) {
        self.clients
            .insert(id, Client::new(sender))
            .assert_is_none();
    }

    fn receive_client_disconnected(&mut self, deps: &mut DepsT, id: ClientId) {
        self.cache.client_disconnected(id);

        let client = self.clients.remove(&id).unwrap();
        for execution in client.executions.into_values() {
            for artifact in execution.acquired_artifacts {
                self.cache.decrement_refcount(artifact);
            }
        }

        self.queued_requests
            .retain(|ExecutionId(cid, _)| *cid != id);
        for worker in self.workers.0.values_mut() {
            worker.pending.retain(|eid| {
                eid.0 != id || {
                    deps.send_message_to_worker(
                        &mut worker.sender,
                        BrokerToWorker::CancelExecution(*eid),
                    );
                    false
                }
            });
        }
        self.worker_heap.rebuild(&mut self.workers);
        self.possibly_start_executions(deps);
    }

    fn receive_client_execution_request(
        &mut self,
        deps: &mut DepsT,
        cid: ClientId,
        ceid: ClientExecutionId,
        details: ExecutionDetails,
    ) {
        let client = self.clients.get_mut(&cid).unwrap();
        let mut execution = Execution::new(details);
        let eid = ExecutionId(cid, ceid);
        for layer in &execution.details.layers {
            match self.cache.get_artifact(eid, layer.clone()) {
                GetArtifact::Success => {
                    execution
                        .acquired_artifacts
                        .insert(layer.clone())
                        .assert_is_true();
                }
                GetArtifact::Wait => {
                    execution
                        .missing_artifacts
                        .insert(layer.clone())
                        .assert_is_true();
                }
                GetArtifact::Get => {
                    execution
                        .missing_artifacts
                        .insert(layer.clone())
                        .assert_is_true();
                    deps.send_message_to_client(
                        &mut client.sender,
                        BrokerToClient::TransferArtifact(layer.clone()),
                    );
                }
            }
        }
        let have_all_artifacts = execution.missing_artifacts.is_empty();
        client.executions.insert(ceid, execution).assert_is_none();
        if have_all_artifacts {
            self.queued_requests.push_back(eid);
            self.possibly_start_executions(deps);
        }
    }

    fn receive_client_statistics_request(&mut self, deps: &mut DepsT, cid: ClientId) {
        let resp = BrokerToClient::StatisticsResponse(BrokerStatistics {
            num_clients: self.clients.len() as u64,
            num_workers: self.workers.0.len() as u64,
            num_requests: self.queued_requests.len() as u64,
        });
        deps.send_message_to_client(&mut self.clients.get_mut(&cid).unwrap().sender, resp);
    }

    fn receive_worker_connected(
        &mut self,
        deps: &mut DepsT,
        id: WorkerId,
        slots: usize,
        sender: DepsT::WorkerSender,
    ) {
        self.workers
            .0
            .insert(id, Worker::new(slots, sender))
            .assert_is_none();
        self.worker_heap.push(&mut self.workers, id);
        self.possibly_start_executions(deps);
    }

    fn receive_worker_disconnected(&mut self, deps: &mut DepsT, id: WorkerId) {
        let mut worker = self.workers.0.remove(&id).unwrap();
        self.worker_heap
            .remove(&mut self.workers, worker.heap_index);

        // We sort the requests to keep our tests deterministic.
        let mut vec: Vec<_> = worker.pending.drain().collect();
        vec.sort();
        for eid in vec.into_iter().rev() {
            self.queued_requests.push_front(eid);
        }

        self.possibly_start_executions(deps);
    }

    fn receive_worker_response(
        &mut self,
        deps: &mut DepsT,
        wid: WorkerId,
        eid: ExecutionId,
        result: ExecutionResult,
    ) {
        let worker = self.workers.0.get_mut(&wid).unwrap();

        if !worker.pending.remove(&eid) {
            // This indicates that the client isn't around anymore. Just ignore this response from
            // the worker. When the client disconnected, we canceled all of the outstanding
            // requests and updated our version of the worker's pending requests.
            return;
        }

        let client = self.clients.get_mut(&eid.0).unwrap();
        deps.send_message_to_client(
            &mut client.sender,
            BrokerToClient::ExecutionResponse(eid.1, result),
        );
        let execution = client.executions.remove(&eid.1).unwrap();
        for artifact in execution.acquired_artifacts {
            self.cache.decrement_refcount(artifact);
        }

        if let Some(eid) = self.queued_requests.pop_front() {
            let details = &self
                .clients
                .get(&eid.0)
                .unwrap()
                .executions
                .get(&eid.1)
                .unwrap()
                .details;
            // If there are any queued_requests, we can just pop one off of the front of
            // the queue and not have to update the worker's used slot count or position in the
            // workers list.
            deps.send_message_to_worker(
                &mut worker.sender,
                BrokerToWorker::EnqueueExecution(eid, details.clone()),
            );
            worker.pending.insert(eid);
        } else {
            // Since there are no queued_requests, we're going to have to update the
            // worker's position in the workers list.
            let heap_index = worker.heap_index;
            self.worker_heap.sift_up(&mut self.workers, heap_index);
        }
    }

    fn receive_got_artifact(
        &mut self,
        deps: &mut DepsT,
        digest: Sha256Digest,
        path: PathBuf,
        bytes_used: u64,
    ) {
        for eid in self.cache.got_artifact(digest.clone(), &path, bytes_used) {
            let execution = self
                .clients
                .get_mut(&eid.0)
                .unwrap()
                .executions
                .get_mut(&eid.1)
                .unwrap();
            execution
                .acquired_artifacts
                .insert(digest.clone())
                .assert_is_true();
            execution.missing_artifacts.remove(&digest).assert_is_true();
            if execution.missing_artifacts.is_empty() {
                self.queued_requests.push_back(eid);
            }
        }
        self.possibly_start_executions(deps);
    }

    fn receive_get_artifact_for_worker(
        &mut self,
        deps: &mut DepsT,
        digest: Sha256Digest,
        mut sender: DepsT::WorkerArtifactFetcherSender,
    ) {
        deps.send_message_to_worker_artifact_fetcher(
            &mut sender,
            self.cache.get_artifact_for_worker(&digest),
        );
    }

    fn receive_decrement_refcount(&mut self, digest: Sha256Digest) {
        self.cache.decrement_refcount(digest);
    }
}

/*  _            _
 * | |_ ___  ___| |_ ___
 * | __/ _ \/ __| __/ __|
 * | ||  __/\__ \ |_\__ \
 *  \__\___||___/\__|___/
 *  FIGLET: tests
 */

#[cfg(test)]
mod tests {
    use super::{Message::*, *};
    use itertools::Itertools;
    use meticulous_base::proto::BrokerToWorker::{self, *};
    use meticulous_test::*;
    use std::{cell::RefCell, sync::Arc};

    #[derive(Clone, Debug, PartialEq)]
    enum TestMessage {
        ToClient(ClientId, BrokerToClient),
        ToWorker(WorkerId, BrokerToWorker),
        ToWorkerArtifactFetcher(u32, Option<PathBuf>),
        CacheGetArtifact(ExecutionId, Sha256Digest),
        CacheGotArtifact(Sha256Digest, PathBuf, u64),
        CacheDecrementRefcount(Sha256Digest),
        CacheClientDisconnected(ClientId),
        CacheGetArtifactForWorker(Sha256Digest),
    }

    use TestMessage::*;

    struct TestClientSender(ClientId);
    struct TestWorkerSender(WorkerId);
    struct TestWorkerArtifactFetcherSender(u32);

    #[derive(Default)]
    struct TestState {
        messages: Vec<TestMessage>,
        get_artifact_returns: HashMap<(ExecutionId, Sha256Digest), Vec<GetArtifact>>,
        got_artifact_returns: HashMap<Sha256Digest, Vec<Vec<ExecutionId>>>,
        get_artifact_for_worker_returns: HashMap<Sha256Digest, Vec<Option<PathBuf>>>,
    }

    impl SchedulerCache for Arc<RefCell<TestState>> {
        fn get_artifact(&mut self, eid: ExecutionId, digest: Sha256Digest) -> GetArtifact {
            self.borrow_mut()
                .messages
                .push(CacheGetArtifact(eid, digest.clone()));
            self.borrow_mut()
                .get_artifact_returns
                .get_mut(&(eid, digest))
                .unwrap()
                .remove(0)
        }
        fn got_artifact(
            &mut self,
            digest: Sha256Digest,
            path: &Path,
            bytes_used: u64,
        ) -> Vec<ExecutionId> {
            self.borrow_mut().messages.push(CacheGotArtifact(
                digest.clone(),
                path.to_owned(),
                bytes_used,
            ));
            self.borrow_mut()
                .got_artifact_returns
                .get_mut(&digest)
                .unwrap()
                .remove(0)
        }
        fn decrement_refcount(&mut self, digest: Sha256Digest) {
            self.borrow_mut()
                .messages
                .push(CacheDecrementRefcount(digest));
        }
        fn client_disconnected(&mut self, cid: ClientId) {
            self.borrow_mut()
                .messages
                .push(CacheClientDisconnected(cid));
        }
        fn get_artifact_for_worker(&mut self, digest: &Sha256Digest) -> Option<PathBuf> {
            self.borrow_mut()
                .messages
                .push(CacheGetArtifactForWorker(digest.clone()));
            self.borrow_mut()
                .get_artifact_for_worker_returns
                .get_mut(&digest)
                .unwrap()
                .remove(0)
        }
    }

    impl SchedulerDeps for Arc<RefCell<TestState>> {
        type ClientSender = TestClientSender;
        type WorkerSender = TestWorkerSender;
        type WorkerArtifactFetcherSender = TestWorkerArtifactFetcherSender;

        fn send_message_to_client(
            &mut self,
            sender: &mut TestClientSender,
            message: BrokerToClient,
        ) {
            self.borrow_mut().messages.push(ToClient(sender.0, message));
        }

        fn send_message_to_worker(
            &mut self,
            sender: &mut TestWorkerSender,
            message: BrokerToWorker,
        ) {
            self.borrow_mut().messages.push(ToWorker(sender.0, message));
        }

        fn send_message_to_worker_artifact_fetcher(
            &mut self,
            sender: &mut TestWorkerArtifactFetcherSender,
            message: Option<PathBuf>,
        ) {
            self.borrow_mut()
                .messages
                .push(ToWorkerArtifactFetcher(sender.0, message));
        }
    }

    struct Fixture {
        test_state: Arc<RefCell<TestState>>,
        scheduler: Scheduler<Arc<RefCell<TestState>>, Arc<RefCell<TestState>>>,
    }

    impl Default for Fixture {
        fn default() -> Self {
            let test_state = Arc::new(RefCell::new(TestState::default()));
            Fixture {
                test_state: test_state.clone(),
                scheduler: Scheduler::new(test_state),
            }
        }
    }

    impl Fixture {
        fn new<const L: usize, const M: usize, const N: usize>(
            get_artifact_returns: [((ExecutionId, Sha256Digest), Vec<GetArtifact>); L],
            got_artifact_returns: [(Sha256Digest, Vec<Vec<ExecutionId>>); M],
            get_artifact_for_worker_returns: [(Sha256Digest, Vec<Option<PathBuf>>); N],
        ) -> Self {
            let result = Self::default();
            result.test_state.borrow_mut().get_artifact_returns =
                HashMap::from(get_artifact_returns);
            result.test_state.borrow_mut().got_artifact_returns =
                HashMap::from(got_artifact_returns);
            result
                .test_state
                .borrow_mut()
                .get_artifact_for_worker_returns = HashMap::from(get_artifact_for_worker_returns);
            result
        }

        fn expect_messages_in_any_order(&mut self, expected: Vec<TestMessage>) {
            let messages = &mut self.test_state.borrow_mut().messages;
            for perm in expected.clone().into_iter().permutations(expected.len()) {
                if perm == *messages {
                    messages.clear();
                    return;
                }
            }
            panic!(
                "Expected messages didn't match actual messages in any order.\n\
                 Expected: {expected:#?}\nActual: {messages:#?}"
            );
        }

        fn receive_message(&mut self, msg: Message<Arc<RefCell<TestState>>>) {
            self.scheduler.receive_message(&mut self.test_state, msg);
        }
    }

    macro_rules! client_sender {
        [$n:expr] => { TestClientSender(cid![$n]) };
    }

    macro_rules! worker_sender {
        [$n:expr] => { TestWorkerSender(wid![$n]) };
    }

    macro_rules! worker_artifact_fetcher_sender {
        [$n:expr] => { TestWorkerArtifactFetcherSender($n) };
    }

    macro_rules! script_test {
        ($test_name:ident, $($in_msg:expr => { $($out_msg:expr),* $(,)? });+ $(;)?) => {
            #[test]
            fn $test_name() {
                let mut fixture = Fixture::default();
                $(
                    fixture.receive_message($in_msg);
                    fixture.expect_messages_in_any_order(vec![$($out_msg,)*]);
                )+
            }
        };
        ($test_name:ident, $fixture_expr:expr, $($in_msg:expr => { $($out_msg:expr),* $(,)? });+ $(;)?) => {
            #[test]
            fn $test_name() {
                let mut fixture = $fixture_expr;
                $(
                    fixture.receive_message($in_msg);
                    fixture.expect_messages_in_any_order(vec![$($out_msg,)*]);
                )+
            }
        };
    }

    script_test! {
        message_from_known_client_ok,
        ClientConnected(cid![1], client_sender![1]) => {};
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![1], details![1])) => {};
    }

    #[test]
    #[should_panic]
    fn request_from_unknown_client_panics() {
        let mut fixture = Fixture::default();
        fixture.receive_message(FromClient(
            cid![1],
            ClientToBroker::ExecutionRequest(ceid![1], details![1]),
        ));
    }

    #[test]
    #[should_panic]
    fn disconnect_from_unknown_client_panics() {
        let mut fixture = Fixture::default();
        fixture.receive_message(ClientDisconnected(cid![1]));
    }

    #[test]
    #[should_panic]
    fn connect_from_duplicate_client_panics() {
        let mut fixture = Fixture::default();
        fixture.receive_message(ClientDisconnected(cid![1]));
        fixture.receive_message(ClientDisconnected(cid![1]));
    }

    #[test]
    #[should_panic]
    fn response_from_unknown_worker_panics() {
        let mut fixture = Fixture::default();
        // The response will be ignored unless we use a valid ClientId.
        fixture.receive_message(ClientConnected(cid![1], client_sender![1]));

        fixture.receive_message(FromWorker(wid![1], WorkerToBroker(eid![1], result![1])));
    }

    #[test]
    #[should_panic]
    fn disconnect_from_unknown_worker_panics() {
        let mut fixture = Fixture::default();
        fixture.receive_message(WorkerDisconnected(wid![1]));
    }

    #[test]
    #[should_panic]
    fn connect_from_duplicate_worker_panics() {
        let mut fixture = Fixture::default();
        fixture.receive_message(WorkerConnected(wid![1], 2, worker_sender![1]));
        fixture.receive_message(WorkerConnected(wid![1], 2, worker_sender![1]));
    }

    script_test! {
        response_from_known_worker_for_unknown_execution_ignored,
        WorkerConnected(wid![1], 2, worker_sender![1]) => {};
        FromWorker(wid![1], WorkerToBroker(eid![1], result![1])) => {};
    }

    script_test! {
        one_client_one_worker,
        ClientConnected(cid![1], client_sender![1]) => {};
        WorkerConnected(wid![1], 2, worker_sender![1]) => {};
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![1], details![1])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1], details![1])),
        };
        FromWorker(wid![1], WorkerToBroker(eid![1], result![1])) => {
            ToClient(cid![1], BrokerToClient::ExecutionResponse(ceid![1], result![1])),
        };
    }

    script_test! {
        response_from_worker_for_disconnected_client_ignored,
        WorkerConnected(wid![1], 2, worker_sender![1]) => {};
        FromWorker(wid![1], WorkerToBroker(eid![1], result![1])) => {};
    }

    script_test! {
        requests_go_to_workers_based_on_subscription_percentage,
        WorkerConnected(wid![1], 2, worker_sender![1]) => {};
        WorkerConnected(wid![2], 2, worker_sender![2]) => {};
        WorkerConnected(wid![3], 3, worker_sender![3]) => {};
        ClientConnected(cid![1], client_sender![1]) => {};

        // 0/2 0/2 0/3
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![1], details![1])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 1], details![1])),
        };

        // 1/2 0/2 0/3
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![2], details![2])) => {
            ToWorker(wid![2], EnqueueExecution(eid![1, 2], details![2])),
        };

        // 1/2 1/2 0/3
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![3], details![3])) => {
            ToWorker(wid![3], EnqueueExecution(eid![1, 3], details![3])),
        };

        // 1/2 1/2 1/3
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![4], details![4])) => {
            ToWorker(wid![3], EnqueueExecution(eid![1, 4], details![4])),
        };

        // 1/2 1/2 2/3
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![5], details![5])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 5], details![5])),
        };

        // 2/2 1/2 2/3
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![6], details![6])) => {
            ToWorker(wid![2], EnqueueExecution(eid![1, 6], details![6])),
        };

        // 2/2 2/2 2/3
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![7], details![7])) => {
            ToWorker(wid![3], EnqueueExecution(eid![1, 7], details![7])),
        };

        FromWorker(wid![1], WorkerToBroker(eid![1, 1], result![1])) => {
            ToClient(cid![1], BrokerToClient::ExecutionResponse(ceid![1], result![1])),
        };
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![8], details![8])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 8], details![8])),
        };

        FromWorker(wid![2], WorkerToBroker(eid![1, 2], result![2])) => {
            ToClient(cid![1], BrokerToClient::ExecutionResponse(ceid![2], result![2])),
        };
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![9], details![9])) => {
            ToWorker(wid![2], EnqueueExecution(eid![1, 9], details![9])),
        };

        FromWorker(wid![3], WorkerToBroker(eid![1, 3], result![3])) => {
            ToClient(cid![1], BrokerToClient::ExecutionResponse(ceid![3], result![3])),
        };
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![10], details![10])) => {
            ToWorker(wid![3], EnqueueExecution(eid![1, 10], details![10])),
        };
    }

    script_test! {
        requests_start_queueing_at_2x_workers_slot_count,
        WorkerConnected(wid![1], 1, worker_sender![1]) => {};
        WorkerConnected(wid![2], 1, worker_sender![2]) => {};
        ClientConnected(cid![1], client_sender![1]) => {};

        // 0/1 0/1
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![1], details![1])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 1], details![1])),
        };

        // 1/1 0/1
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![2], details![2])) => {
            ToWorker(wid![2], EnqueueExecution(eid![1, 2], details![2])),
        };

        // 1/1 1/1
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![3], details![3])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 3], details![3])),
        };

        // 2/1 1/1
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![4], details![4])) => {
            ToWorker(wid![2], EnqueueExecution(eid![1, 4], details![4])),
        };

        // 2/1 2/1
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![5], details![5])) => {};
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![6], details![6])) => {};

        // 2/2 1/2
        FromWorker(wid![2], WorkerToBroker(eid![1, 2], result![2])) => {
            ToClient(cid![1], BrokerToClient::ExecutionResponse(ceid![2], result![2])),
            ToWorker(wid![2], EnqueueExecution(eid![1, 5], details![5])),
        };

        // 1/2 2/2
        FromWorker(wid![1], WorkerToBroker(eid![1, 1], result![1])) => {
            ToClient(cid![1], BrokerToClient::ExecutionResponse(ceid![1], result![1])),
            ToWorker(wid![1], EnqueueExecution(eid![1, 6], details![6])),
        };
    }

    script_test! {
        queued_requests_go_to_workers_on_connect,
        ClientConnected(cid![1], client_sender![1]) => {};

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![1], details![1])) => {};
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![2], details![2])) => {};
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![3], details![3])) => {};
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![4], details![4])) => {};
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![5], details![5])) => {};
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![6], details![6])) => {};

        WorkerConnected(wid![1], 2, worker_sender![1]) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 1], details![1])),
            ToWorker(wid![1], EnqueueExecution(eid![1, 2], details![2])),
            ToWorker(wid![1], EnqueueExecution(eid![1, 3], details![3])),
            ToWorker(wid![1], EnqueueExecution(eid![1, 4], details![4])),
        };

        WorkerConnected(wid![2], 2, worker_sender![2]) => {
            ToWorker(wid![2], EnqueueExecution(eid![1, 5], details![5])),
            ToWorker(wid![2], EnqueueExecution(eid![1, 6], details![6])),
        };
    }

    script_test! {
        requests_outstanding_on_disconnected_worker_get_sent_to_new_workers,
        WorkerConnected(wid![1], 1, worker_sender![1]) => {};
        WorkerConnected(wid![2], 1, worker_sender![2]) => {};
        WorkerConnected(wid![3], 1, worker_sender![3]) => {};
        ClientConnected(cid![1], client_sender![1]) => {};

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![1], details![1])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 1], details![1])),
        };

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![2], details![2])) => {
            ToWorker(wid![2], EnqueueExecution(eid![1, 2], details![2])),
        };

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![3], details![3])) => {
            ToWorker(wid![3], EnqueueExecution(eid![1, 3], details![3])),
        };

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![4], details![4])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 4], details![4])),
        };

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![5], details![5])) => {
            ToWorker(wid![2], EnqueueExecution(eid![1, 5], details![5])),
        };

        WorkerDisconnected(wid![1]) => {
            ToWorker(wid![3], EnqueueExecution(eid![1, 1], details![1])),
        };

        FromWorker(wid![2], WorkerToBroker(eid![1, 2], result![2])) => {
            ToClient(cid![1], BrokerToClient::ExecutionResponse(ceid![2], result![2])),
            ToWorker(wid![2], EnqueueExecution(eid![1, 4], details![4])),
        };
    }

    script_test! {
        requests_outstanding_on_disconnected_worker_get_sent_to_new_workers_2,
        WorkerConnected(wid![1], 1, worker_sender![1]) => {};
        ClientConnected(cid![1], client_sender![1]) => {};

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![1], details![1])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 1], details![1])),
        };

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![2], details![2])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 2], details![2])),
        };

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![3], details![3])) => {};
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![4], details![4])) => {};

        FromWorker(wid![1], WorkerToBroker(eid![1, 1], result![1])) => {
            ToClient(cid![1], BrokerToClient::ExecutionResponse(ceid![1], result![1])),
            ToWorker(wid![1], EnqueueExecution(eid![1, 3], details![3])),
        };

        WorkerConnected(wid![2], 1, worker_sender![2]) => {
            ToWorker(wid![2], EnqueueExecution(eid![1, 4], details![4])),
        };

        WorkerDisconnected(wid![1]) => {
            ToWorker(wid![2], EnqueueExecution(eid![1, 2], details![2])),
        };

        FromWorker(wid![2], WorkerToBroker(eid![1, 2], result![2])) => {
            ToClient(cid![1], BrokerToClient::ExecutionResponse(ceid![2], result![2])),
            ToWorker(wid![2], EnqueueExecution(eid![1, 3], details![3])),
        };
    }

    script_test! {
        requests_outstanding_on_disconnected_worker_go_to_head_of_queue_for_other_workers,
        WorkerConnected(wid![1], 1, worker_sender![1]) => {};
        ClientConnected(cid![1], client_sender![1]) => {};

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![1], details![1])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 1], details![1])),
        };

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![2], details![2])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 2], details![2])),
        };

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![3], details![3])) => {};
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![4], details![4])) => {};

        WorkerDisconnected(wid![1]) => {};

        WorkerConnected(wid![2], 1, worker_sender![2]) => {
            ToWorker(wid![2], EnqueueExecution(eid![1, 1], details![1])),
            ToWorker(wid![2], EnqueueExecution(eid![1, 2], details![2])),
        };
    }

    script_test! {
        requests_get_removed_from_workers_pending_map,

        WorkerConnected(wid![1], 1, worker_sender![1]) => {};
        ClientConnected(cid![1], client_sender![1]) => {};

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![1], details![1])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 1], details![1])),
        };

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![2], details![2])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 2], details![2])),
        };

        FromWorker(wid![1], WorkerToBroker(eid![1, 1], result![1])) => {
            ToClient(cid![1], BrokerToClient::ExecutionResponse(ceid![1], result![1])),
        };

        FromWorker(wid![1], WorkerToBroker(eid![1, 2], result![1])) => {
            ToClient(cid![1], BrokerToClient::ExecutionResponse(ceid![2], result![1])),
        };

        WorkerDisconnected(wid![1]) => {};
        WorkerConnected(wid![2], 1, worker_sender![2]) => {};
    }

    script_test! {
        client_disconnects_with_outstanding_work_1,
        WorkerConnected(wid![1], 1, worker_sender![1]) => {};
        ClientConnected(cid![1], client_sender![1]) => {};

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![1], details![1])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 1], details![1])),
        };

        ClientDisconnected(cid![1]) => {
            ToWorker(wid![1], CancelExecution(eid![1, 1])),
            CacheClientDisconnected(cid![1]),
        };
    }

    script_test! {
        client_disconnects_with_outstanding_work_2,
        WorkerConnected(wid![1], 1, worker_sender![1]) => {};
        WorkerConnected(wid![2], 1, worker_sender![2]) => {};
        ClientConnected(cid![1], client_sender![1]) => {};
        ClientConnected(cid![2], client_sender![2]) => {};

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![1], details![1])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 1], details![1])),
        };

        FromClient(cid![2], ClientToBroker::ExecutionRequest(ceid![1], details![1])) => {
            ToWorker(wid![2], EnqueueExecution(eid![2, 1], details![1])),
        };

        //ClientDisconnected(cid![2]) => {
        //    ToWorker(wid![2], CancelExecution(eid![2, 1])),
        //};

        //FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![2], details![2])) => {
        //    ToWorker(wid![2], EnqueueExecution(eid![1, 2], details![2])),
        //};
    }

    script_test! {
        client_disconnects_with_outstanding_work_3,
        WorkerConnected(wid![1], 1, worker_sender![1]) => {};
        ClientConnected(cid![1], client_sender![1]) => {};

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![1], details![1])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 1], details![1])),
        };

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![2], details![2])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 2], details![2])),
        };

        ClientConnected(cid![2], client_sender![2]) => {};
        FromClient(cid![2], ClientToBroker::ExecutionRequest(ceid![1], details![1])) => {};
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![3], details![3])) => {};

        ClientDisconnected(cid![2]) => {
            CacheClientDisconnected(cid![2]),
        };

        FromWorker(wid![1], WorkerToBroker(eid![1, 1], result![1])) => {
            ToClient(cid![1], BrokerToClient::ExecutionResponse(ceid![1], result![1])),
            ToWorker(wid![1], EnqueueExecution(eid![1, 3], details![3])),
        };
    }

    script_test! {
        client_disconnects_with_outstanding_work_4,
        WorkerConnected(wid![1], 1, worker_sender![1]) => {};
        WorkerConnected(wid![2], 1, worker_sender![2]) => {};
        ClientConnected(cid![1], client_sender![1]) => {};
        ClientConnected(cid![2], client_sender![2]) => {};

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![1], details![1])) => {
            ToWorker(wid![1], EnqueueExecution(eid![1, 1], details![1])),
        };

        FromClient(cid![2], ClientToBroker::ExecutionRequest(ceid![1], details![1])) => {
            ToWorker(wid![2], EnqueueExecution(eid![2, 1], details![1])),
        };

        FromClient(cid![2], ClientToBroker::ExecutionRequest(ceid![2], details![2])) => {
            ToWorker(wid![1], EnqueueExecution(eid![2, 2], details![2])),
        };

        FromClient(cid![2], ClientToBroker::ExecutionRequest(ceid![3], details![3])) => {
            ToWorker(wid![2], EnqueueExecution(eid![2, 3], details![3])),
        };

        FromClient(cid![2], ClientToBroker::ExecutionRequest(ceid![4], details![4])) => {};
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![2], details![2])) => {};
        FromClient(cid![2], ClientToBroker::ExecutionRequest(ceid![5], details![5])) => {};
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![3], details![3])) => {};
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![4], details![4])) => {};
        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![5], details![5])) => {};

        ClientDisconnected(cid![2]) => {
            ToWorker(wid![2], CancelExecution(eid![2, 1])),
            ToWorker(wid![1], CancelExecution(eid![2, 2])),
            ToWorker(wid![2], CancelExecution(eid![2, 3])),

            ToWorker(wid![2], EnqueueExecution(eid![1, 2], details![2])),
            ToWorker(wid![1], EnqueueExecution(eid![1, 3], details![3])),
            ToWorker(wid![2], EnqueueExecution(eid![1, 4], details![4])),

            CacheClientDisconnected(cid![2]),
        };
    }

    script_test! {
        request_with_layers,
        {
            Fixture::new([
                ((eid![1, 2], digest![42]), vec![GetArtifact::Success]),
                ((eid![1, 2], digest![43]), vec![GetArtifact::Wait]),
                ((eid![1, 2], digest![44]), vec![GetArtifact::Get]),
            ],
            [],
            [])
        },
        WorkerConnected(wid![1], 1, worker_sender![1]) => {};
        ClientConnected(cid![1], client_sender![1]) => {};

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![2], details![1, [42, 43, 44]])) => {
            CacheGetArtifact(eid![1, 2], digest![42]),
            CacheGetArtifact(eid![1, 2], digest![43]),
            CacheGetArtifact(eid![1, 2], digest![44]),
            ToClient(cid![1], BrokerToClient::TransferArtifact(digest![44])),
        };

        ClientDisconnected(cid![1]) => {
            CacheClientDisconnected(cid![1]),
            CacheDecrementRefcount(digest![42]),
        }
    }

    script_test! {
        request_with_layers_2,
        {
            Fixture::new([
                ((eid![1, 2], digest![42]), vec![GetArtifact::Success]),
                ((eid![1, 2], digest![43]), vec![GetArtifact::Success]),
                ((eid![1, 2], digest![44]), vec![GetArtifact::Success]),
            ],
            [],
            [])
        },
        WorkerConnected(wid![1], 1, worker_sender![1]) => {};
        ClientConnected(cid![1], client_sender![1]) => {};

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![2], details![1, [42, 43, 44]])) => {
            CacheGetArtifact(eid![1, 2], digest![42]),
            CacheGetArtifact(eid![1, 2], digest![43]),
            CacheGetArtifact(eid![1, 2], digest![44]),
            ToWorker(wid![1], EnqueueExecution(eid![1, 2], details![1, [42, 43, 44]])),
        };

        FromWorker(wid![1], WorkerToBroker(eid![1, 2], result![1])) => {
            ToClient(cid![1], BrokerToClient::ExecutionResponse(ceid![2], result![1])),
            CacheDecrementRefcount(digest![42]),
            CacheDecrementRefcount(digest![43]),
            CacheDecrementRefcount(digest![44]),
        };
    }

    script_test! {
        request_with_layers_3,
        {
            Fixture::new([
                ((eid![1, 2], digest![42]), vec![GetArtifact::Success]),
                ((eid![1, 2], digest![43]), vec![GetArtifact::Wait]),
                ((eid![1, 2], digest![44]), vec![GetArtifact::Get]),
            ],
            [
                (digest![43], vec![vec![eid![1, 2]]]),
                (digest![44], vec![vec![eid![1, 2]]]),
            ],
            [])
        },
        WorkerConnected(wid![1], 1, worker_sender![1]) => {};
        ClientConnected(cid![1], client_sender![1]) => {};

        FromClient(cid![1], ClientToBroker::ExecutionRequest(ceid![2], details![1, [42, 43, 44]])) => {
            CacheGetArtifact(eid![1, 2], digest![42]),
            CacheGetArtifact(eid![1, 2], digest![43]),
            CacheGetArtifact(eid![1, 2], digest![44]),
            ToClient(cid![1], BrokerToClient::TransferArtifact(digest![44])),
        };

        GotArtifact(digest![43], "/z/tmp/foo".into(), 100) => {
            CacheGotArtifact(digest![43], "/z/tmp/foo".into(),100),
        };
        GotArtifact(digest![44], "/z/tmp/bar".into(), 100) => {
            CacheGotArtifact(digest![44], "/z/tmp/bar".into(),100),
            ToWorker(wid![1], EnqueueExecution(eid![1, 2], details![1, [42, 43, 44]])),
        };

        ClientDisconnected(cid![1]) => {
            ToWorker(wid![1], CancelExecution(eid![1, 2])),
            CacheClientDisconnected(cid![1]),
            CacheDecrementRefcount(digest![42]),
            CacheDecrementRefcount(digest![43]),
            CacheDecrementRefcount(digest![44]),
        }
    }

    script_test! {
        get_artifact_for_worker,
        {
            Fixture::new([], [], [(digest![42], vec![Some("/a/good/path".into())])])
        },
        GetArtifactForWorker(digest![42], worker_artifact_fetcher_sender![1]) => {
            CacheGetArtifactForWorker(digest![42]),
            ToWorkerArtifactFetcher(1, Some("/a/good/path".into())),
        }
    }

    script_test! {
        decrement_refcount,
        DecrementRefcount(digest![42]) => {
            CacheDecrementRefcount(digest![42]),
        }
    }
}

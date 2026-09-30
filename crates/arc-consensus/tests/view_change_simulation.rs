//! Deterministic multi-node simulation of the authenticated round-skip and
//! finality protocol (checklist C5).
//!
//! No sockets, no clocks, no threads: every node owns a real `ConsensusEngine`
//! and a real `SkipTracker`, and a scripted network decides what each node sees
//! and when. The same seed always produces the same run, so a failure here is
//! reproducible rather than a flake.
//!
//! Two assertions run over every scenario:
//!
//! * **safety** — no two nodes ever commit different blocks for the same round,
//!   and no round is ever both committed and skipped anywhere;
//! * **liveness** — after the stated faults heal, the commit cursor strictly
//!   advances. A scenario that merely reproduces a permanent stall does not
//!   pass this gate.

use std::collections::{BTreeMap, HashMap, HashSet};

use arc_consensus::view_change::{
    AbsenceReason, ConsensusSigningRecord, DEFAULT_SKIP_GRACE_MS, FinalityCertificate,
    FinalityVote, FinalityVoteCollector, RECENT_ABSENCE_GRACE_MS, SkipCertificate, SkipTracker,
    SkipVote, SkipVoteCollector, validator_set_hash,
};
use arc_consensus::{
    ConsensusDomain, ConsensusEngine, DagBlock, STAKE_ARC, Validator, ValidatorSet,
};
use arc_crypto::{Hash256, KeyPair, hash_bytes};
use arc_types::Address;

const GRACE: u64 = DEFAULT_SKIP_GRACE_MS;
/// Simulated milliseconds per tick. Two ticks clear the grace period.
const TICK_MS: u64 = GRACE;
/// How many of its own recent blocks each node re-gossips per tick.
const GOSSIP_REPEAT: usize = 6;
/// Bound on one history transfer in the simulation.
const HISTORY_SPAN: u64 = 256;

fn domain() -> ConsensusDomain {
    ConsensusDomain::new(hash_bytes(b"arc.sim.domain.v1"), 1, 1)
}

fn committee(n: usize) -> (ValidatorSet, Vec<KeyPair>) {
    // Deterministic keys: the simulation must replay identically.
    let keys: Vec<KeyPair> = (0..n)
        .map(|i| {
            let seed = hash_bytes(format!("arc.sim.validator.{i}").as_bytes());
            KeyPair::from_ed25519_secret_bytes(&seed.0)
        })
        .collect();
    let validators: Vec<Validator> = keys
        .iter()
        .enumerate()
        .map(|(i, key)| Validator::new(key.address(), STAKE_ARC, i as u16).expect("valid"))
        .collect();
    (ValidatorSet::new(validators, 1), keys)
}

/// `try_commit`'s leader rule, mirrored so the simulation knows who is due.
fn leader_for_round(set: &ValidatorSet, round: u64) -> Address {
    let mut addrs: Vec<Address> = set.validators.iter().map(|v| v.address).collect();
    addrs.sort_by_key(|a| a.0);
    addrs[round as usize % addrs.len()]
}

struct Node {
    index: usize,
    address: Address,
    keypair: KeyPair,
    engine: ConsensusEngine,
    tracker: SkipTracker,
    skip_votes: SkipVoteCollector,
    finality_votes: FinalityVoteCollector,
    /// round -> committed block hash, as this node saw it.
    committed: BTreeMap<u64, Hash256>,
    /// Rounds this node passed on an authenticated certificate.
    skipped: HashSet<u64>,
    /// Certificates this node holds and will gossip.
    held_certificates: Vec<SkipCertificate>,
    /// This node's own recent blocks, re-gossiped each tick. A real transport
    /// re-sends on reconnect and peers re-request; without modelling that, a
    /// node that was down for one tick would never learn a block at all, and
    /// the scenario would be testing the simulation rather than the protocol.
    own_blocks: Vec<DagBlock>,
    finality: BTreeMap<u64, FinalityCertificate>,
    /// Set to false to model a crashed or partitioned-away node.
    online: bool,
    /// The round this node was at on the previous tick, used to notice that it
    /// is stuck and should ask for history.
    stuck_at: Option<u64>,
}

impl Node {
    fn new(index: usize, keypair: KeyPair, set: &ValidatorSet) -> Self {
        let engine =
            ConsensusEngine::new_with_keypair(set.clone(), keypair.address(), keypair.clone());
        engine
            .install_consensus_domain(domain())
            .expect("fresh engine binds its domain");
        let tracker = SkipTracker::new(
            domain(),
            validator_set_hash(set),
            GRACE,
            ConsensusSigningRecord::default(),
        );
        Self {
            index,
            address: keypair.address(),
            keypair,
            engine,
            tracker,
            skip_votes: SkipVoteCollector::new(),
            finality_votes: FinalityVoteCollector::new(),
            committed: BTreeMap::new(),
            skipped: HashSet::new(),
            held_certificates: Vec::new(),
            own_blocks: Vec::new(),
            finality: BTreeMap::new(),
            online: true,
            stuck_at: None,
        }
    }

    /// Restart: the engine's in-memory DAG is lost, but the durable signing
    /// record is reloaded, which is the whole point of persisting it.
    fn restart(&mut self, set: &ValidatorSet) {
        let record = self.tracker.record().clone();
        let engine = ConsensusEngine::new_with_keypair(
            set.clone(),
            self.keypair.address(),
            self.keypair.clone(),
        );
        engine
            .install_consensus_domain(domain())
            .expect("fresh engine binds its domain");
        // The durable record preserves observations, not quorum certificates.
        // Certificates must be received and verified again after restart.
        self.engine = engine;
        self.tracker = SkipTracker::new(domain(), validator_set_hash(set), GRACE, record);
        self.skip_votes = SkipVoteCollector::new();
        self.online = true;
    }
}

/// What a node decides to send this tick.
#[derive(Clone)]
enum Msg {
    Block(DagBlock),
    /// A node that is behind asks everyone for history from a round.
    HistoryRequest {
        from_round: u64,
    },
    Skip(SkipVote),
    Cert(SkipCertificate),
    Finality(FinalityVote),
}

/// Scripted faults, evaluated per tick.
#[derive(Default, Clone)]
struct Faults {
    /// Node indices that produce nothing and receive nothing.
    offline: HashSet<usize>,
    /// Ordered pairs `(from, to)` whose messages are dropped.
    cut: HashSet<(usize, usize)>,
    /// Deliver every message twice.
    duplicate: bool,
    /// Deliver this tick's messages in reverse order.
    reorder: bool,
    /// Hold this tick's messages back and deliver them one tick later.
    delay: bool,
}

struct Sim {
    set: ValidatorSet,
    nodes: Vec<Node>,
    now_ms: u64,
    tick_ms: u64,
    /// Messages held back by a `delay` tick.
    pending: Vec<(usize, Msg)>,
}

impl Sim {
    fn new(n: usize) -> Self {
        let (set, keys) = committee(n);
        let nodes = keys
            .iter()
            .enumerate()
            .map(|(i, key)| Node::new(i, key.clone(), &set))
            .collect();
        Sim {
            set,
            nodes,
            now_ms: 0,
            tick_ms: TICK_MS,
            pending: Vec::new(),
        }
    }

    fn quorum(&self) -> u64 {
        self.set.quorum
    }

    /// One tick: every online node proposes for its current round, then all
    /// messages are delivered under the tick's fault script, then every node
    /// tries to advance, skip, commit and finalise.
    fn tick(&mut self, faults: &Faults) {
        self.now_ms += self.tick_ms;
        let mut outbox: Vec<(usize, Msg)> = std::mem::take(&mut self.pending);

        // ── propose ──────────────────────────────────────────────────────────
        for index in 0..self.nodes.len() {
            if faults.offline.contains(&index) || !self.nodes[index].online {
                continue;
            }
            // Proposing for the node's own current round; the engine decides it.
            let timestamp = 1_000_000 + self.now_ms + index as u64;
            if let Ok(block) = self.nodes[index].engine.propose_block(vec![], timestamp) {
                self.nodes[index].own_blocks.push(block.clone());
                outbox.push((index, Msg::Block(block)));
            }
            // Gossip redundancy: re-send this node's recent blocks so a peer
            // that was offline or slow can still receive them.
            let recent = self.nodes[index]
                .own_blocks
                .len()
                .saturating_sub(GOSSIP_REPEAT);
            for block in self.nodes[index].own_blocks[recent..].iter().cloned() {
                outbox.push((index, Msg::Block(block)));
            }
        }

        // ── skip votes ───────────────────────────────────────────────────────
        for index in 0..self.nodes.len() {
            if faults.offline.contains(&index) || !self.nodes[index].online {
                continue;
            }
            let cursor = self.nodes[index].engine.last_committed_round();
            let current = self.nodes[index].engine.current_round();
            // Candidates are every round from the commit cursor up to and
            // INCLUDING the current one. Excluding the current round would
            // deadlock: the round cannot advance while the absent leader is
            // still required, and the certificate that excuses it can only be
            // built from observations of that same round.
            for round in cursor..=current {
                let blocks = self.nodes[index].engine.blocks_in_round(round);
                let mut seen_authors = HashSet::new();
                let mut stake = 0u64;
                for hash in &blocks {
                    if let Some(block) = self.nodes[index].engine.get_block(hash)
                        && seen_authors.insert(block.author)
                        && let Some(validator) = self.set.get_validator(&block.author)
                    {
                        stake += validator.stake;
                    }
                }
                let quorum = self.set.quorum;
                let now = self.now_ms;
                // Certify the absence of every member missing from this round,
                // not only the leader: the recovery domain requires a block
                // from every fixed validator before the round can advance, and
                // the certificate is what excuses one.
                let members: Vec<Address> = self.set.validators.iter().map(|v| v.address).collect();
                for member in members {
                    if self.nodes[index]
                        .engine
                        .has_skip_certificate(round, &member)
                    {
                        continue;
                    }
                    let seen = seen_authors.contains(&member);
                    let keypair = self.nodes[index].keypair.clone();
                    // (a) the member produced nothing this round
                    self.nodes[index].tracker.observe(
                        round,
                        &member,
                        AbsenceReason::NoBlock,
                        stake,
                        !seen,
                        quorum,
                        now,
                    );
                    if let Ok(vote) = self.nodes[index].tracker.sign_if_permitted(
                        round,
                        member,
                        AbsenceReason::NoBlock,
                        cursor,
                        quorum,
                        now,
                        &keypair,
                    ) {
                        // S5: the decision is already in the record; a real node
                        // fsyncs it here, before the vote leaves the process.
                        outbox.push((index, Msg::Skip(vote)));
                        continue;
                    }
                }
            }
        }

        // ── a node that is behind asks for history ───────────────────────────
        // A node whose round has not moved while peers are live cannot be
        // helped by gossip: a block more than one round ahead is refused. This
        // is the same trigger the node implements.
        for index in 0..self.nodes.len() {
            if faults.offline.contains(&index) || !self.nodes[index].online {
                continue;
            }
            let round = self.nodes[index].engine.current_round();
            if self.nodes[index].stuck_at == Some(round) {
                outbox.push((index, Msg::HistoryRequest { from_round: round }));
            }
            self.nodes[index].stuck_at = Some(round);
        }

        // ── certificates this node already holds get re-gossiped ─────────────
        for index in 0..self.nodes.len() {
            if faults.offline.contains(&index) || !self.nodes[index].online {
                continue;
            }
            let certs = self.nodes[index].held_certificates.clone();
            for certificate in certs {
                outbox.push((index, Msg::Cert(certificate)));
            }
        }

        if faults.delay {
            self.pending = outbox;
            return;
        }
        if faults.reorder {
            outbox.reverse();
        }
        if faults.duplicate {
            let copy = outbox.clone();
            outbox.extend(copy);
        }

        // ── deliver ──────────────────────────────────────────────────────────
        let mut responses: Vec<(usize, usize, Vec<DagBlock>)> = Vec::new();
        for (from, msg) in &outbox {
            for to in 0..self.nodes.len() {
                if faults.offline.contains(&to) || !self.nodes[to].online {
                    continue;
                }
                if from != &to && faults.cut.contains(&(*from, to)) {
                    continue;
                }
                match msg {
                    Msg::Block(block) => {
                        let _ = self.nodes[to].engine.receive_block(block);
                    }
                    Msg::HistoryRequest { from_round } => {
                        // Serve from what this node actually holds, bounded.
                        let mut blocks = Vec::new();
                        for round in *from_round..from_round.saturating_add(HISTORY_SPAN) {
                            for hash in self.nodes[to].engine.blocks_in_round(round) {
                                if let Some(block) = self.nodes[to].engine.get_block(&hash) {
                                    blocks.push(block);
                                }
                            }
                        }
                        if !blocks.is_empty() {
                            responses.push((to, *from, blocks));
                        }
                    }
                    Msg::Skip(vote) => {
                        let set = self.set.clone();
                        if let Ok(Some(certificate)) =
                            self.nodes[to].skip_votes.add(vote.clone(), &domain(), &set)
                            && self.nodes[to]
                                .engine
                                .register_skip_certificate(certificate.clone())
                                .is_ok()
                        {
                            self.nodes[to].tracker.adopt_certificate(&certificate);
                            self.nodes[to].held_certificates.push(certificate);
                        }
                    }
                    Msg::Cert(certificate) => {
                        if !self.nodes[to]
                            .engine
                            .has_skip_certificate(certificate.round, &certificate.absentee)
                            && self.nodes[to]
                                .engine
                                .register_skip_certificate(certificate.clone())
                                .is_ok()
                        {
                            self.nodes[to].tracker.adopt_certificate(certificate);
                            self.nodes[to].held_certificates.push(certificate.clone());
                        }
                    }
                    Msg::Finality(vote) => {
                        let set = self.set.clone();
                        if let Ok(Some(certificate)) =
                            self.nodes[to]
                                .finality_votes
                                .add(vote.clone(), &domain(), &set)
                        {
                            let height = certificate.height;
                            if self.nodes[to]
                                .engine
                                .register_finality_certificate(certificate.clone())
                                .is_ok()
                            {
                                self.nodes[to].finality.insert(height, certificate);
                            }
                        }
                    }
                }
            }
        }

        // History answers are delivered on the same tick, which models a
        // request/response exchange rather than gossip.
        for (from, to, blocks) in responses {
            if faults.offline.contains(&to) || !self.nodes[to].online {
                continue;
            }
            if from != to && faults.cut.contains(&(from, to)) {
                continue;
            }
            let _ = self.nodes[to].engine.import_history(&blocks, HISTORY_SPAN);
        }

        // ── advance, commit, finalise ────────────────────────────────────────
        let mut finality_votes = Vec::new();
        for index in 0..self.nodes.len() {
            if faults.offline.contains(&index) || !self.nodes[index].online {
                continue;
            }
            let _ = self.nodes[index].engine.advance_round();
            let before = self.nodes[index].engine.last_committed_round();
            let committed = self.nodes[index].engine.try_commit();
            for block in &committed {
                self.nodes[index].committed.insert(block.round, block.hash);
                // Finality is signed only after the commit condition holds.
                let vote = FinalityVote::sign(
                    domain(),
                    validator_set_hash(&self.set),
                    block.round,
                    block.hash,
                    hash_bytes(format!("state-{}", block.round).as_bytes()),
                    hash_bytes(format!("tx-{}", block.round).as_bytes()),
                    &self.nodes[index].keypair,
                )
                .expect("finality vote signs");
                finality_votes.push((index, vote));
            }
            let after = self.nodes[index].engine.last_committed_round();
            for round in before..after {
                if !self.nodes[index].committed.contains_key(&round) {
                    self.nodes[index].skipped.insert(round);
                }
            }
        }
        for (from, vote) in finality_votes {
            self.pending.push((from, Msg::Finality(vote)));
        }
    }

    fn run(&mut self, ticks: usize, faults: &Faults) {
        for _ in 0..ticks {
            self.tick(faults);
        }
    }

    /// The commit cursor every online node has reached.
    fn cursors(&self) -> Vec<u64> {
        self.nodes
            .iter()
            .filter(|node| node.online)
            .map(|node| node.engine.last_committed_round())
            .collect()
    }

    /// SAFETY: no two nodes disagree about what a round committed, and no round
    /// is both committed somewhere and skipped somewhere.
    fn assert_safety(&self, label: &str) {
        let mut decided: HashMap<u64, Hash256> = HashMap::new();
        for node in &self.nodes {
            for (round, hash) in &node.committed {
                if let Some(existing) = decided.get(round) {
                    assert_eq!(
                        existing, hash,
                        "{label}: nodes disagree about the block committed at round {round}"
                    );
                } else {
                    decided.insert(*round, *hash);
                }
            }
        }
        for node in &self.nodes {
            for round in &node.skipped {
                assert!(
                    !decided.contains_key(round),
                    "{label}: node {} skipped round {round}, which node(s) committed",
                    node.index
                );
            }
        }
    }

    /// LIVENESS: at least the given cursor was reached by every online node.
    fn assert_progress(&self, label: &str, at_least: u64) {
        for node in self.nodes.iter().filter(|n| n.online) {
            let cursor = node.engine.last_committed_round();
            assert!(
                cursor >= at_least,
                "{label}: node {} cursor {cursor} did not reach {at_least}",
                node.index
            );
        }
    }
}

// ── scenarios ────────────────────────────────────────────────────────────────

#[test]
fn healthy_committees_commit_without_any_skip() {
    for n in [4usize, 5, 6] {
        let mut sim = Sim::new(n);
        sim.run(20, &Faults::default());
        sim.assert_safety("healthy");
        sim.assert_progress("healthy", 3);
        for node in &sim.nodes {
            assert!(
                node.skipped.is_empty(),
                "n={n}: a healthy committee skipped round(s) {:?}",
                node.skipped
            );
        }
    }
}

#[test]
fn a_permanently_silent_leader_is_skipped_and_the_chain_keeps_committing() {
    // This is the defect the protocol exists to fix: at N>=4 quorum is smaller
    // than the committee, so one silent member used to halt commits for good.
    for n in [4usize, 5, 6] {
        let mut sim = Sim::new(n);
        // Silence whichever node leads round 1, so rounds 0 and 2.. are normal
        // and exactly one leader slot is empty per rotation.
        let silent_leader = leader_for_round(&sim.set, 1);
        let silent = sim
            .nodes
            .iter()
            .position(|node| node.address == silent_leader)
            .expect("leader is in the committee");
        let faults = Faults {
            offline: HashSet::from([silent]),
            ..Default::default()
        };
        sim.run(40, &faults);
        sim.assert_safety("silent leader");

        let online_cursors: Vec<u64> = sim
            .nodes
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != silent)
            .map(|(_, node)| node.engine.last_committed_round())
            .collect();
        assert!(
            online_cursors.iter().all(|cursor| *cursor > 1),
            "n={n}: the cursor never passed the silent leader's round: {online_cursors:?}"
        );
        let skipped_somewhere = sim.nodes.iter().any(|node| node.skipped.contains(&1));
        assert!(
            skipped_somewhere,
            "n={n}: round 1 was neither committed nor certified-skipped"
        );
    }
}

#[test]
fn adaptive_absence_grace_triples_throughput_with_a_silent_member() {
    let committed_rounds = |grace_ms| {
        let mut sim = Sim::new(6);
        sim.tick_ms = 100;
        let silent = leader_for_round(&sim.set, 1);
        for node in &mut sim.nodes {
            node.online = node.address != silent;
            node.tracker = SkipTracker::new(
                domain(),
                validator_set_hash(&sim.set),
                GRACE,
                ConsensusSigningRecord::default(),
            )
            .with_recent_absence_grace(grace_ms);
        }
        // Both policies get the same 20 simulated seconds, including the first
        // absence's full grace. Every live validator must commit the same sequence.
        sim.run(200, &Faults::default());
        assert_eq!(sim.now_ms, 20_000);
        sim.assert_safety("silent member throughput");
        let mut online = sim.nodes.iter().filter(|node| node.online);
        let first = online.next().expect("five live validators");
        for node in online {
            assert_eq!(node.committed, first.committed);
        }
        first.committed.len()
    };
    let full = committed_rounds(GRACE);
    let adaptive = committed_rounds(RECENT_ABSENCE_GRACE_MS);
    assert!(full > 0, "the full-grace policy must also make progress");
    assert!(
        adaptive >= 3 * full,
        "adaptive committed {adaptive} rounds versus {full} with full grace in 20 seconds"
    );
}

#[test]
fn delayed_reordered_and_duplicated_messages_do_not_break_safety_or_progress() {
    let mut sim = Sim::new(4);
    let hostile = Faults {
        duplicate: true,
        reorder: true,
        ..Default::default()
    };
    sim.run(10, &hostile);
    sim.assert_safety("duplicate+reorder");
    // A tick of pure delay, then healing.
    let delayed = Faults {
        delay: true,
        ..Default::default()
    };
    sim.run(2, &delayed);
    sim.run(20, &Faults::default());
    sim.assert_safety("after delay heals");
    sim.assert_progress("after delay heals", 3);
}

#[test]
fn a_minority_partition_stalls_and_then_heals_without_conflicting_commits() {
    let mut sim = Sim::new(6);
    sim.run(8, &Faults::default());
    let before = sim.cursors();

    // Six equal validators have quorum 20,000,001 against a total of
    // 30,000,000, so five are required. Isolating ONE leaves the majority at
    // exactly quorum; isolating two would stall the whole chain and would
    // measure the committee rather than the protocol.
    let mut cut = HashSet::new();
    for isolated in [5usize] {
        for other in 0..6usize {
            if other != isolated {
                cut.insert((isolated, other));
                cut.insert((other, isolated));
            }
        }
    }
    let partition = Faults {
        cut,
        ..Default::default()
    };
    sim.run(15, &partition);
    sim.assert_safety("partitioned");

    // Heal. The majority must have kept moving and the minority must not have
    // committed anything conflicting while it was isolated.
    sim.run(25, &Faults::default());
    sim.assert_safety("healed");
    let after = sim.cursors();
    assert!(
        after.iter().max().unwrap() > before.iter().max().unwrap(),
        "the majority did not make progress across the partition: {before:?} -> {after:?}"
    );
}

#[test]
fn an_equivocating_skip_voter_cannot_forge_a_certificate() {
    // A Byzantine validator signs a skip for a leader nobody else names. With
    // stake below quorum it can never assemble a certificate, and its votes are
    // rejected outright when they contradict the certificate header.
    let (set, keys) = committee(4);
    let liar = &keys[3];
    let set_hash = validator_set_hash(&set);
    let real_leader = leader_for_round(&set, 5);
    let fake_leader = keys
        .iter()
        .map(|k| k.address())
        .find(|a| *a != real_leader)
        .unwrap();

    let mut collector = SkipVoteCollector::new();
    for _ in 0..10 {
        let vote = SkipVote::sign(
            domain(),
            set_hash,
            5,
            fake_leader,
            AbsenceReason::NoBlock,
            set.quorum,
            liar,
        )
        .unwrap();
        let outcome = collector.add(vote, &domain(), &set).unwrap();
        assert!(
            outcome.is_none(),
            "one repeated voter must never reach quorum"
        );
    }

    // Two honest voters plus the liar's vote for a DIFFERENT leader must not
    // combine: the collector keys by (round, leader), so they never mix.
    for key in keys.iter().take(2) {
        let vote = SkipVote::sign(
            domain(),
            set_hash,
            5,
            real_leader,
            AbsenceReason::NoBlock,
            set.quorum,
            key,
        )
        .unwrap();
        assert!(collector.add(vote, &domain(), &set).unwrap().is_none());
    }
}

#[test]
fn a_stale_certificate_from_another_committee_is_rejected_by_every_node() {
    let mut sim = Sim::new(4);
    // A genuinely different committee: different size, therefore a different
    // membership commitment.
    let (other_set, other_keys) = committee(5);
    let leader = leader_for_round(&other_set, 2);
    let votes: Vec<SkipVote> = other_keys[..3]
        .iter()
        .map(|key| {
            SkipVote::sign(
                domain(),
                validator_set_hash(&other_set),
                2,
                leader,
                AbsenceReason::NoBlock,
                other_set.quorum,
                key,
            )
            .unwrap()
        })
        .collect();
    let forged = SkipCertificate::new(
        domain(),
        validator_set_hash(&other_set),
        2,
        leader,
        AbsenceReason::NoBlock,
        votes,
    );
    for node in &mut sim.nodes {
        assert!(
            node.engine
                .register_skip_certificate(forged.clone())
                .is_err(),
            "a certificate from another committee must never register"
        );
        assert!(!node.engine.has_skip_certificate(2, &leader));
    }
}

#[test]
fn local_absence_vote_and_reloaded_observation_do_not_excuse_participation() {
    let mut sim = Sim::new(6);
    let missing = sim.nodes[5].address;
    let mut faults = Faults {
        offline: HashSet::from([5]),
        ..Default::default()
    };
    // Deliver five signed bootstrap blocks, then let every observer start its
    // grace clock. Before votes form, isolate node 0 from all other voters.
    sim.tick(&faults);
    sim.tick(&faults);
    faults.cut = (1..6).map(|from| (from, 0)).collect();
    sim.tick(&faults);
    let record = sim.nodes[0].tracker.record().clone();
    assert_eq!(record.skipped_rounds[&0][&missing], AbsenceReason::NoBlock);
    assert!(!sim.nodes[0].engine.has_skip_certificate(0, &missing));
    assert_eq!(
        sim.nodes[0].engine.current_round(),
        0,
        "one durable local observation must not advance the recovery round"
    );
    assert!(!sim.nodes[0].engine.is_certified_absent(0, &missing));
    assert!(sim.nodes[0].engine.try_commit().is_empty());

    // Peers with a real certificate can produce a five-parent child. The
    // isolated node must not accept that same signed child or import it as
    // complete history on the strength of its own observation.
    let peer_child = sim.nodes[1].engine.propose_block(vec![], 40_000).unwrap();
    assert!(sim.nodes[0].engine.receive_block(&peer_child).is_err());
    assert!(sim.nodes[0].engine.get_block(&peer_child.hash).is_none());

    let parents: Vec<_> = sim.nodes[0]
        .engine
        .blocks_in_round(0)
        .into_iter()
        .map(|hash| sim.nodes[0].engine.get_block(&hash).unwrap())
        .collect();
    let mut offered_history = parents.clone();
    offered_history.push(peer_child.clone());
    let _ = sim.nodes[0]
        .engine
        .import_history(&offered_history, HISTORY_SPAN);
    assert_eq!(sim.nodes[0].engine.current_round(), 0);
    assert!(sim.nodes[0].engine.get_block(&peer_child.hash).is_none());
    let set = sim.set.clone();
    sim.nodes[0].restart(&set);
    assert_eq!(
        sim.nodes[0].tracker.record().skipped_rounds,
        record.skipped_rounds
    );
    for block in &parents {
        sim.nodes[0].engine.receive_block(block).unwrap();
    }
    assert!(
        !sim.nodes[0].engine.advance_round(),
        "reopen must not upgrade a persisted observation into a certificate"
    );
    assert!(!sim.nodes[0].engine.is_certified_absent(0, &missing));

    let votes: Vec<_> = sim
        .nodes
        .iter()
        .take(5)
        .map(|node| {
            SkipVote::sign(
                domain(),
                validator_set_hash(&set),
                0,
                missing,
                AbsenceReason::NoBlock,
                5 * STAKE_ARC,
                &node.keypair,
            )
            .unwrap()
        })
        .collect();
    let insufficient = SkipCertificate::new(
        domain(),
        validator_set_hash(&set),
        0,
        missing,
        AbsenceReason::NoBlock,
        votes[..4].to_vec(),
    );
    assert!(
        sim.nodes[0]
            .engine
            .register_skip_certificate(insufficient)
            .is_err()
    );
    assert!(!sim.nodes[0].engine.advance_round());
    let certified = SkipCertificate::new(
        domain(),
        validator_set_hash(&set),
        0,
        missing,
        AbsenceReason::NoBlock,
        votes,
    );
    sim.nodes[0]
        .engine
        .register_skip_certificate(certified)
        .unwrap();
    assert!(sim.nodes[0].engine.is_certified_absent(0, &missing));
    assert!(sim.nodes[0].engine.advance_round());
    sim.nodes[0].engine.receive_block(&peer_child).unwrap();
    let child = sim.nodes[0].engine.propose_block(vec![], 50_000).unwrap();
    assert_eq!(child.parents.len(), 5);
    assert!(sim.nodes[0].engine.try_commit().is_empty());

    // Absence is an observation, not a permanent block refusal. Keep the
    // durable vote while accepting the absentee's delayed authentic block.
    let late = sim.nodes[5].engine.propose_block(vec![], 50_001).unwrap();
    sim.nodes[0].engine.receive_block(&late).unwrap();
    assert_eq!(
        sim.nodes[0].tracker.record().skipped_rounds,
        record.skipped_rounds
    );
}

#[test]
fn a_restarted_node_keeps_its_refusals_and_rejoins_without_conflicting() {
    let mut sim = Sim::new(4);
    let silent_leader = leader_for_round(&sim.set, 1);
    let silent = sim
        .nodes
        .iter()
        .position(|node| node.address == silent_leader)
        .unwrap();
    let faults = Faults {
        offline: HashSet::from([silent]),
        ..Default::default()
    };
    sim.run(20, &faults);
    sim.assert_safety("before restart");

    // Restart a node that has already signed skips. Its in-memory DAG is gone;
    // its durable signing record is not.
    let restart_index = (silent + 1) % sim.nodes.len();
    let refusals_before: Vec<(u64, Address)> = sim.nodes[restart_index]
        .tracker
        .record()
        .skipped_rounds
        .iter()
        .flat_map(|(round, members)| members.keys().map(move |member| (*round, *member)))
        .collect();
    assert!(
        !refusals_before.is_empty(),
        "the scenario must have produced at least one skip to restart across"
    );
    let set = sim.set.clone();
    sim.nodes[restart_index].restart(&set);
    for (round, leader) in &refusals_before {
        assert!(
            sim.nodes[restart_index].tracker.refuses(*round, leader),
            "a restarted node forgot that it voted to skip round {round}"
        );
        assert!(
            !sim.nodes[restart_index]
                .engine
                .is_certified_absent(*round, leader),
            "a restarted engine must reacquire a verified certificate for round {round}"
        );
    }

    sim.run(20, &faults);
    sim.assert_safety("after restart");
}

#[test]
fn finality_certificates_form_over_committed_blocks_and_verify_independently() {
    let mut sim = Sim::new(4);
    sim.run(25, &Faults::default());
    sim.assert_safety("finality run");

    let with_finality = sim
        .nodes
        .iter()
        .filter(|node| !node.finality.is_empty())
        .count();
    assert!(
        with_finality > 0,
        "no node assembled a finality certificate over a committed block"
    );

    // Independent verification: a certificate must verify against the frozen
    // committee alone, with no access to the node that produced it.
    for node in &sim.nodes {
        for (height, certificate) in &node.finality {
            let signing = certificate
                .verify(&domain(), &sim.set)
                .expect("finality certificate verifies against the frozen committee");
            assert!(signing >= sim.quorum());
            // And it describes a block that was genuinely committed there.
            assert_eq!(
                node.committed.get(height),
                Some(&certificate.block_hash),
                "a finality certificate names a block this node never committed"
            );
        }
    }
}

#[test]
fn a_finality_certificate_cannot_be_assembled_below_quorum() {
    let (set, keys) = committee(6);
    let set_hash = validator_set_hash(&set);
    let mut collector = FinalityVoteCollector::new();
    let block = hash_bytes(b"b");
    let state = hash_bytes(b"s");
    let tx = hash_bytes(b"t");
    // Five of six is quorum for this committee; four must not be.
    let f = set.total_stake - set.quorum;
    let below = ((set.quorum - 1) / STAKE_ARC) as usize;
    assert!(f > 0);
    for key in keys.iter().take(below) {
        let vote = FinalityVote::sign(domain(), set_hash, 4, block, state, tx, key).unwrap();
        assert!(
            collector.add(vote, &domain(), &set).unwrap().is_none(),
            "{below} voters must not reach quorum"
        );
    }
    let vote = FinalityVote::sign(domain(), set_hash, 4, block, state, tx, &keys[below]).unwrap();
    let certificate = collector
        .add(vote, &domain(), &set)
        .unwrap()
        .expect("quorum reached");
    assert!(certificate.verify(&domain(), &set).unwrap() >= set.quorum);
}

#[test]
#[ignore = "diagnostic: prints per-tick state for the silent-leader scenario"]
fn diagnostic_silent_leader_trace() {
    let mut sim = Sim::new(4);
    let silent_leader = leader_for_round(&sim.set, 1);
    let silent = sim
        .nodes
        .iter()
        .position(|node| node.address == silent_leader)
        .unwrap();
    let faults = Faults {
        offline: HashSet::from([silent]),
        ..Default::default()
    };
    eprintln!("silent index {silent} address {silent_leader}");
    eprintln!("quorum {} total {}", sim.set.quorum, sim.set.total_stake);
    for tick in 0..8 {
        sim.tick(&faults);
        let states: Vec<String> = sim
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| {
                format!(
                    "n{i}[r={} c={} blocks0={} certs={}]",
                    node.engine.current_round(),
                    node.engine.last_committed_round(),
                    node.engine.blocks_in_round(0).len(),
                    node.tracker.record().skipped_rounds.len(),
                )
            })
            .collect();
        eprintln!("tick {tick}: {}", states.join(" "));
    }
}

#[test]
fn a_staggered_start_still_commits() {
    // Real nodes do not start at the same instant: the fixture starts node i
    // after node i-1 so that simultaneous mutual dialling cannot deadlock
    // (defect D1). That makes the first few rounds ragged - a leader's block
    // exists but only a minority of the next round references it - which is a
    // different situation from an absent leader and is NOT covered by an
    // absence certificate. This asserts the chain still reaches a commit.
    for n in [4usize, 6] {
        let mut sim = Sim::new(n);
        for started in 1..=n {
            let offline: HashSet<usize> = (started..n).collect();
            sim.run(
                2,
                &Faults {
                    offline,
                    ..Default::default()
                },
            );
        }
        sim.run(40, &Faults::default());
        sim.assert_safety("staggered start");
        let cursors = sim.cursors();
        assert!(
            cursors.iter().all(|cursor| *cursor > 0),
            "n={n}: a staggered start never reached a commit: {cursors:?}"
        );
    }
}

#[test]
#[ignore = "diagnostic: staggered-start commit state"]
fn diagnostic_staggered_trace() {
    let n = 4usize;
    let mut sim = Sim::new(n);
    for started in 1..=n {
        let offline: HashSet<usize> = (started..n).collect();
        sim.run(
            2,
            &Faults {
                offline,
                ..Default::default()
            },
        );
    }
    sim.run(20, &Faults::default());
    for (i, node) in sim.nodes.iter().enumerate() {
        let r = node.engine.current_round();
        let c = node.engine.last_committed_round();
        let counts: Vec<usize> = (0..4)
            .map(|k| node.engine.blocks_in_round(k).len())
            .collect();
        eprintln!(
            "n{i}: round={r} cursor={c} blocks(0..4)={counts:?} certs={}",
            node.tracker.record().skipped_rounds.len()
        );
    }
    let node = &sim.nodes[0];
    for round in 0..4u64 {
        let leader = leader_for_round(&sim.set, round);
        let hashes = node.engine.blocks_in_round(round);
        let leader_block = hashes.iter().copied().find(|h| {
            node.engine
                .get_block(h)
                .map(|b| b.author == leader)
                .unwrap_or(false)
        });
        match leader_block {
            Some(hash) => {
                let support = node.engine.leader_commit_support(&hash, round);
                let parents_of_next: Vec<usize> = node
                    .engine
                    .blocks_in_round(round + 1)
                    .iter()
                    .filter_map(|h| node.engine.get_block(h))
                    .map(|b| b.parents.len())
                    .collect();
                eprintln!(
                    "round {round}: leader block present, support={support:?} quorum={} next-round parent counts={parents_of_next:?}",
                    sim.set.quorum
                );
            }
            None => eprintln!("round {round}: leader block ABSENT"),
        }
    }
}

#[test]
fn a_node_that_starts_late_joins_the_running_chain() {
    // Defect D5: a node that falls behind rejects every live block as "round N
    // is too far ahead" and never rejoins, because gossip can only carry it one
    // round. The gossip window here is deliberately exhausted before the late
    // node appears, so ONLY authenticated history transfer can rescue it.
    for n in [4usize, 6] {
        let mut sim = Sim::new(n);
        let late = n - 1;
        let faults = Faults {
            offline: HashSet::from([late]),
            ..Default::default()
        };
        // Run long enough that the late node's peers are far past the one-round
        // window AND past the re-gossip window.
        sim.run(60, &faults);
        let ahead = sim.nodes[0].engine.current_round();
        assert!(
            ahead > GOSSIP_REPEAT as u64 + 1,
            "n={n}: the chain did not get far enough ahead to make this a real test"
        );

        sim.nodes[late].online = true;
        sim.run(30, &Faults::default());
        sim.assert_safety("late join");

        let joined = sim.nodes[late].engine.current_round();
        assert!(
            joined > 1,
            "n={n}: the late node never joined - it is still at round {joined} while its \
             peers are at {}",
            sim.nodes[0].engine.current_round()
        );
        assert!(
            sim.nodes[late].engine.last_committed_round() > 0,
            "n={n}: the late node caught up on rounds but committed nothing"
        );
    }
}

#[test]
fn history_import_refuses_a_gap_a_jump_and_a_thin_round() {
    // The import path is the only one that may carry a node forward by more
    // than one round, so its refusals are load-bearing.
    let mut sim = Sim::new(4);
    sim.run(12, &Faults::default());
    let source = &sim.nodes[0];
    let mut all = Vec::new();
    for round in 0..=source.engine.current_round() {
        for hash in source.engine.blocks_in_round(round) {
            if let Some(block) = source.engine.get_block(&hash) {
                all.push(block);
            }
        }
    }
    assert!(all.len() > 8);

    let fresh = Sim::new(4);
    let importer = &fresh.nodes[0].engine;

    // A run that starts above the round this node is waiting for.
    let ahead: Vec<DagBlock> = all.iter().filter(|b| b.round >= 3).cloned().collect();
    assert!(
        importer.import_history(&ahead, HISTORY_SPAN).is_err(),
        "history that starts above the waiting round must be refused"
    );

    // A run with a hole in the middle.
    let holed: Vec<DagBlock> = all.iter().filter(|b| b.round != 2).cloned().collect();
    assert!(
        importer.import_history(&holed, HISTORY_SPAN).is_err(),
        "history with a gap must be refused"
    );

    // Nothing above was imported.
    assert_eq!(importer.current_round(), 0);

    // A round carrying less than quorum stake is never stepped OVER.
    //
    // This used to assert that the whole run was refused. That was the
    // behaviour, and it was the defect: refusing the run also discarded the
    // complete rounds before the thin one, which is what wedged the R8 soak.
    // The property that actually matters for safety is narrower and is what
    // this asserts - the importer may stop AT a thin round but never beyond it.
    let mut thin: Vec<DagBlock> = Vec::new();
    for round in 0..=2u64 {
        for (taken, block) in all.iter().filter(|b| b.round == round).enumerate() {
            if round == 1 && taken >= 1 {
                break;
            }
            thin.push(block.clone());
        }
    }
    let thin_importer_sim = Sim::new(4);
    let thin_importer = &thin_importer_sim.nodes[0].engine;
    let _ = thin_importer.import_history(&thin, HISTORY_SPAN);
    assert!(
        thin_importer.current_round() <= 1,
        "the importer reached round {} - past round 1, which carries less than \
         quorum stake",
        thin_importer.current_round()
    );
    assert!(
        thin_importer.blocks_in_round(2).is_empty(),
        "nothing above the thin round may be imported"
    );

    // A run whose FIRST round is thin has nothing before it to keep, so it is
    // refused outright.
    let thin_first: Vec<DagBlock> = all
        .iter()
        .filter(|b| b.round == 0)
        .take(1)
        .cloned()
        .collect();
    assert!(
        Sim::new(4).nodes[0]
            .engine
            .import_history(&thin_first, HISTORY_SPAN)
            .is_err(),
        "a run that is thin from its first round must be refused"
    );

    // The complete, contiguous run is accepted.
    let reached = importer
        .import_history(&all, HISTORY_SPAN)
        .expect("a complete contiguous run imports");
    assert!(reached > 1, "import reached only round {reached}");
}

#[test]
fn many_absences_in_one_round_cannot_stop_block_production() {
    // Regression for the CRITICAL liveness defect an independent adversarial
    // review found in the first design: attestations refused blocks as parents,
    // nothing bounded how much stake could be attested absent in one round, and
    // once the refused stake exceeded f every proposer failed with
    // InsufficientParents - permanently, since the record forbids forgetting.
    //
    // Certificates no longer refuse anything, so this drives the same shape and
    // requires the chain to keep producing AND committing.
    let n = 4usize;
    let mut sim = Sim::new(n);
    sim.run(6, &Faults::default());

    // Force the precondition directly: excuse every member of several rounds,
    // which is strictly more than the review's interleaving could achieve.
    let members: Vec<Address> = sim.set.validators.iter().map(|v| v.address).collect();
    for round in 0..6u64 {
        for node in &sim.nodes {
            for member in &members {
                let votes = sim
                    .nodes
                    .iter()
                    .map(|voter| {
                        SkipVote::sign(
                            domain(),
                            validator_set_hash(&sim.set),
                            round,
                            *member,
                            AbsenceReason::NoBlock,
                            sim.set.total_stake,
                            &voter.keypair,
                        )
                        .unwrap()
                    })
                    .collect();
                node.engine
                    .register_skip_certificate(SkipCertificate::new(
                        domain(),
                        validator_set_hash(&sim.set),
                        round,
                        *member,
                        AbsenceReason::NoBlock,
                        votes,
                    ))
                    .unwrap();
            }
        }
    }

    let before = sim.cursors();
    sim.run(30, &Faults::default());
    sim.assert_safety("many absences");
    let after = sim.cursors();
    assert!(
        after.iter().min().unwrap() > before.iter().min().unwrap(),
        "block production stopped after many absences in one round: {before:?} -> {after:?}"
    );
}

#[test]
fn an_anchor_one_node_certifies_is_never_skipped_by_another() {
    // The other CRITICAL finding: honest nodes could disagree about whether a
    // block could ever be committed, because that judgement was made from a
    // local view. The retroactive rule makes it from a later committed anchor's
    // causal history instead, which every node that holds the anchor computes
    // identically. Drive a network with delay, reorder and duplication - the
    // conditions that produced divergent views - and require every node's
    // commit decisions to agree wherever they overlap.
    for n in [4usize, 7] {
        let mut sim = Sim::new(n);
        let hostile = Faults {
            duplicate: true,
            reorder: true,
            ..Default::default()
        };
        for tick in 0..24 {
            let faults = if tick % 3 == 0 {
                Faults {
                    delay: true,
                    ..hostile.clone()
                }
            } else {
                hostile.clone()
            };
            sim.tick(&faults);
        }
        sim.run(30, &Faults::default());
        sim.assert_safety("divergent views");
        // And the chain actually got somewhere, so this is not a vacuous pass.
        assert!(
            sim.cursors().iter().max().unwrap() > &2,
            "n={n}: no progress under hostile delivery"
        );
    }
}

#[test]
fn a_node_with_an_empty_dag_fills_it_in_from_the_first_missing_round() {
    // Regression for the restart case, which took five fixture runs to pin
    // down. A restarted node has a round cursor far ahead of an EMPTY DAG, so
    // every block it needs is BELOW that cursor. Two things then went wrong:
    // an import that inserted hundreds of blocks without moving the cursor was
    // judged a failure, and the next request was computed from a cursor rather
    // than from the gap - so the node re-requested the same range forever.
    let mut sim = Sim::new(4);
    sim.run(30, &Faults::default());
    let source = &sim.nodes[0];
    let top = source.engine.current_round();
    assert!(top > 8, "the source chain must be long enough to matter");

    let mut all = Vec::new();
    for round in 0..=top {
        for hash in source.engine.blocks_in_round(round) {
            if let Some(block) = source.engine.get_block(&hash) {
                all.push(block);
            }
        }
    }

    // A fresh engine standing in for the restarted node: empty DAG, but its
    // round cursor already restored far ahead.
    let fresh = Sim::new(4);
    let engine = &fresh.nodes[0].engine;
    engine.restore_round_from_local_wal(top, top.saturating_sub(3));
    assert!(engine.dag_is_empty());
    assert_eq!(engine.first_missing_round(0, 1_000), 0);

    // Importing rounds entirely below the cursor must count as progress.
    let lower: Vec<DagBlock> = all.iter().filter(|b| b.round <= 4).cloned().collect();
    engine
        .import_history(&lower, HISTORY_SPAN)
        .expect("history below the round cursor is progress, not failure");
    assert!(!engine.dag_is_empty());

    // And the next request must start at the gap, not back at 0 or at a cursor.
    let next = engine.first_missing_round(0, 1_000);
    assert_eq!(
        next, 5,
        "after importing rounds 0..=4 the node must ask from 5, not from 0 or from its cursor"
    );

    // Feeding it the rest walks it all the way up.
    engine
        .import_history(&all, HISTORY_SPAN)
        .expect("the remainder imports");
    assert!(
        engine.first_missing_round(0, 1_000) > next,
        "the DAG did not fill in any further"
    );
}

/// Two quorum certificates at one height naming different blocks is a
/// committee-level safety violation. The engine must surface it, because
/// overwriting the held certificate destroys the only evidence it happened.
///
/// This is the check the C14 divergence could not be evaluated against: the
/// node held certificates in a map keyed by height alone, a second certificate
/// for a height already held was skipped unread, and the evidence line did not
/// name the block.
#[test]
fn conflicting_finality_certificates_at_one_height_are_detected_not_overwritten() {
    let (set, keys) = committee(4);
    let set_hash = validator_set_hash(&set);
    let engine = ConsensusEngine::new_with_keypair(set.clone(), keys[0].address(), keys[0].clone());
    engine
        .install_consensus_domain(domain())
        .expect("fresh engine binds its domain");

    let certify = |block_label: &[u8], voters: &[usize]| -> FinalityCertificate {
        let block = hash_bytes(block_label);
        let state = hash_bytes(b"state-root");
        let tx = hash_bytes(b"tx-root");
        let mut collector = FinalityVoteCollector::new();
        let mut out = None;
        for index in voters {
            let vote = FinalityVote::sign(domain(), set_hash, 77, block, state, tx, &keys[*index])
                .unwrap();
            if let Some(certificate) = collector.add(vote, &domain(), &set).unwrap() {
                out = Some(certificate);
            }
        }
        out.expect("quorum reached")
    };

    // Quorum A certifies one block at height 77.
    let first = certify(b"block-A", &[0, 1, 2]);
    assert_eq!(
        engine
            .register_finality_certificate(first.clone())
            .map(|_| ()),
        Ok(()),
        "the first certificate at a height must be accepted"
    );
    assert_eq!(engine.finalized_block_at(77), Some(first.block_hash));

    // Re-delivering the SAME certificate is benign and must stay accepted -
    // peers legitimately re-gossip, and a restarted node re-emits its votes.
    engine
        .register_finality_certificate(first.clone())
        .expect("a repeat of the held certificate is not a conflict");
    assert_eq!(engine.finalized_block_at(77), Some(first.block_hash));

    // Quorum B - overlapping, as any two quorums must - certifies a different
    // block at the same height.
    let second = certify(b"block-B", &[1, 2, 3]);
    assert_ne!(first.block_hash, second.block_hash);
    let error = engine
        .register_finality_certificate(second.clone())
        .expect_err("a conflicting certificate must not be accepted");
    match error {
        arc_consensus::view_change::CertificateError::ConflictingFinality {
            height,
            held,
            offered,
        } => {
            assert_eq!(height, 77);
            assert_eq!(held, first.block_hash);
            assert_eq!(offered, second.block_hash);
        }
        other => panic!("expected a conflict report, got {other:?}"),
    }

    // The evidence must survive the conflict.
    assert_eq!(
        engine.finalized_block_at(77),
        Some(first.block_hash),
        "the held certificate must not be replaced by the conflicting one"
    );
}

/// What `/finality/{height}` serves must be verifiable by someone who does not
/// trust the server - that is the entire difference between it and
/// `/block/{height}`, which reports one node's own observation.
///
/// This exercises the round trip a light client performs: take the encoded
/// certificate, decode it, and verify it against the committee and chain
/// domain the client already trusts.
#[test]
fn a_served_finality_certificate_verifies_against_an_independently_held_committee() {
    let (set, keys) = committee(4);
    let set_hash = validator_set_hash(&set);
    let engine = ConsensusEngine::new_with_keypair(set.clone(), keys[0].address(), keys[0].clone());
    engine
        .install_consensus_domain(domain())
        .expect("fresh engine binds its domain");

    let block = hash_bytes(b"final-block");
    let state_root = hash_bytes(b"final-state");
    let tx_root = hash_bytes(b"final-txs");
    let mut collector = FinalityVoteCollector::new();
    let mut certificate = None;
    for key in keys.iter().take(3) {
        let vote =
            FinalityVote::sign(domain(), set_hash, 12, block, state_root, tx_root, key).unwrap();
        if let Some(assembled) = collector.add(vote, &domain(), &set).unwrap() {
            certificate = Some(assembled);
        }
    }
    let certificate = certificate.expect("quorum reached");
    engine
        .register_finality_certificate(certificate)
        .expect("accepted");

    // The server side: fetch and encode exactly what the endpoint returns.
    let served = engine
        .finality_certificate(12)
        .expect("the node holds a certificate at this height");
    let encoded = bincode::serialize(&served).expect("certificate encodes");

    // The client side. It holds the committee and the chain domain from its
    // own trusted source, NOT from the response.
    let decoded: FinalityCertificate = bincode::deserialize(&encoded).expect("decodes");
    let signing = decoded
        .verify(&domain(), &set)
        .expect("a served certificate must verify against the real committee");
    assert!(signing >= set.quorum);
    assert_eq!(decoded.block_hash, block);
    assert_eq!(decoded.state_root, state_root);

    // The same bytes must NOT verify against a different committee or a
    // different chain - otherwise the certificate would prove nothing about
    // which chain the client is on.
    let (other_set, _) = {
        let keys: Vec<KeyPair> = (0..4)
            .map(|i| {
                KeyPair::from_ed25519_secret_bytes(
                    &hash_bytes(format!("arc.sim.other.{i}").as_bytes()).0,
                )
            })
            .collect();
        let validators: Vec<Validator> = keys
            .iter()
            .enumerate()
            .map(|(i, key)| Validator::new(key.address(), STAKE_ARC, i as u16).expect("valid"))
            .collect();
        (ValidatorSet::new(validators, 1), keys)
    };
    assert!(
        decoded.verify(&domain(), &other_set).is_err(),
        "a certificate must not verify against a committee that did not sign it"
    );
    let other_domain = ConsensusDomain::new(hash_bytes(b"arc.sim.other.chain"), 1, 1);
    assert!(
        decoded.verify(&other_domain, &set).is_err(),
        "a certificate must not verify against a different chain domain"
    );

    // A node-observed commit is a different fact. The engine holds no
    // certificate at a height it merely committed, and the endpoint must not
    // invent one.
    assert!(
        engine.finality_certificate(13).is_none(),
        "an uncertified height must have no certificate to serve"
    );
}

/// Regression for the liveness wedge the R8 soak self-test found.
///
/// A history response always includes the live tip round, and at a stalled tip
/// that round is BELOW quorum by definition - that is what "stalled" means.
/// The import pre-check rejected the ENTIRE run whenever any round in it was
/// thin, so a node that had fallen behind rejected every response it was ever
/// sent: the complete rounds it needed were discarded along with the one
/// incomplete round at the end.
///
/// In the soak that turned one crash plus one slow node into a permanent halt
/// at N=4. The restarted node rejected the same response 118 times, the laggard
/// could not catch up, and the two of them were exactly the validators whose
/// blocks the tip round needed to reach quorum.
///
/// The comment on the check already said the right thing - a thin round "is
/// the point where the import stops, not a reason to accept the rest" - while
/// the code returned an error. This pins the behaviour to the comment.
#[test]
fn a_thin_tip_round_does_not_discard_the_complete_rounds_before_it() {
    let mut sim = Sim::new(4);
    sim.run(12, &Faults::default());
    let source = &sim.nodes[0];
    let tip = source.engine.current_round();
    assert!(tip >= 4, "need several complete rounds, got tip {tip}");

    // Every complete round, then a final round cut down to ONE block - the
    // shape a peer serves when the tip has stalled below quorum.
    let thin_round = tip - 1;
    let mut run: Vec<DagBlock> = Vec::new();
    for round in 0..thin_round {
        for hash in source.engine.blocks_in_round(round) {
            if let Some(block) = source.engine.get_block(&hash) {
                run.push(block);
            }
        }
    }
    let thin_block = source
        .engine
        .blocks_in_round(thin_round)
        .into_iter()
        .find_map(|hash| source.engine.get_block(&hash))
        .expect("the thin round has at least one block");
    run.push(thin_block);

    let fresh = Sim::new(4);
    let importer = &fresh.nodes[0].engine;
    let reached = importer
        .import_history(&run, HISTORY_SPAN)
        .expect("the complete rounds before a thin tip must import, not be discarded with it");

    assert!(
        reached >= thin_round.saturating_sub(1),
        "import stopped at round {reached}; every complete round below {thin_round} \
         should have been kept"
    );
    // The safety half: a thin round is never stepped OVER. The importer may
    // stand AT it, waiting for its missing blocks, but may not be beyond it.
    assert!(
        importer.current_round() <= thin_round,
        "the importer advanced to round {} - past thin round {thin_round}, which \
         carries less than quorum stake",
        importer.current_round()
    );
}

/// Finality certificates are retained for a bounded window of heights.
///
/// Unbounded, the map grew by one ~1.2 KB certificate per height for the life
/// of the process - roughly half a gigabyte per node per day at the fixture's
/// block rate, which a 24-hour soak would have found the slow way.
#[test]
fn finality_certificates_are_retained_for_a_bounded_window() {
    let (set, keys) = committee(4);
    let set_hash = validator_set_hash(&set);
    let engine = ConsensusEngine::new_with_keypair(set.clone(), keys[0].address(), keys[0].clone());
    engine
        .install_consensus_domain(domain())
        .expect("fresh engine binds its domain");
    engine.set_retained_finality_heights(256);

    let certify = |height: u64, label: &[u8]| -> FinalityCertificate {
        let block = hash_bytes(label);
        let mut collector = FinalityVoteCollector::new();
        let mut out = None;
        for key in keys.iter().take(3) {
            let vote = FinalityVote::sign(
                domain(),
                set_hash,
                height,
                block,
                hash_bytes(b"state"),
                hash_bytes(b"txs"),
                key,
            )
            .unwrap();
            if let Some(c) = collector.add(vote, &domain(), &set).unwrap() {
                out = Some(c);
            }
        }
        out.expect("quorum")
    };

    for height in 1..=2_000u64 {
        engine
            .register_finality_certificate(certify(height, &height.to_le_bytes()))
            .expect("accepted");
    }
    assert_eq!(engine.highest_finalized_height(), Some(2_000));
    let held = engine.finality_certificate_count();
    assert!(
        held <= 256 + 256,
        "{held} certificates held; the window is 256 heights plus one prune interval"
    );
    assert!(
        engine.finality_certificate(2_000).is_some(),
        "the newest certificate must be held"
    );
    assert!(
        engine.finality_certificate(10).is_none(),
        "a certificate far below the window must have been pruned"
    );

    // Conflict detection still works for every height the window retains.
    let conflicting = certify(1_990, b"a-different-block");
    assert!(matches!(
        engine.register_finality_certificate(conflicting),
        Err(arc_consensus::view_change::CertificateError::ConflictingFinality { .. })
    ));
}

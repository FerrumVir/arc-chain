use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProofMarketError {
    RequestNotFound,
    OfferNotFound,
    DuplicateOffer,
    RequestExpired,
    InvalidProof,
    ProverNotFound,
    OfferAlreadyAccepted,
    RequestAlreadyFulfilled,
    InsufficientPayment,
    DeadlinePassed,
}

impl fmt::Display for ProofMarketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequestNotFound => write!(f, "proof request not found"),
            Self::OfferNotFound => write!(f, "proof offer not found"),
            Self::DuplicateOffer => write!(f, "prover already submitted an offer for this request"),
            Self::RequestExpired => write!(f, "proof request has expired"),
            Self::InvalidProof => write!(f, "submitted proof failed verification"),
            Self::ProverNotFound => write!(f, "prover not found in registry"),
            Self::OfferAlreadyAccepted => {
                write!(f, "an offer has already been accepted for this request")
            }
            Self::RequestAlreadyFulfilled => write!(f, "request has already been fulfilled"),
            Self::InsufficientPayment => write!(f, "offered price is below the minimum"),
            Self::DeadlinePassed => write!(f, "deadline for this request has passed"),
        }
    }
}

impl std::error::Error for ProofMarketError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProofPriority {
    Economy,
    Standard,
    Express,
    Instant,
}

impl ProofPriority {
    pub fn cost_multiplier(&self) -> f64 {
        match self {
            Self::Economy => 0.5,
            Self::Standard => 1.0,
            Self::Express => 2.0,
            Self::Instant => 5.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofRequest {
    pub id: [u8; 32],
    pub requester: [u8; 32],
    pub circuit_id: [u8; 32],
    pub public_inputs: Vec<[u8; 32]>,
    pub max_cost: u64,
    pub deadline: u64,
    pub priority: ProofPriority,
}

impl ProofRequest {
    pub fn new(
        id: [u8; 32],
        requester: [u8; 32],
        circuit_id: [u8; 32],
        public_inputs: Vec<[u8; 32]>,
        max_cost: u64,
        deadline: u64,
        priority: ProofPriority,
    ) -> Self {
        Self {
            id,
            requester,
            circuit_id,
            public_inputs,
            max_cost,
            deadline,
            priority,
        }
    }

    pub fn is_expired(&self, current_time: u64) -> bool {
        current_time > self.deadline
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofOffer {
    pub prover: [u8; 32],
    pub request_id: [u8; 32],
    pub estimated_time_ms: u64,
    pub price: u64,
    pub prover_reputation: f64,
}

impl ProofOffer {
    pub fn new(
        prover: [u8; 32],
        request_id: [u8; 32],
        estimated_time_ms: u64,
        price: u64,
        prover_reputation: f64,
    ) -> Self {
        Self {
            prover,
            request_id,
            estimated_time_ms,
            price,
            prover_reputation,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofSubmission {
    pub request_id: [u8; 32],
    pub prover: [u8; 32],
    pub proof_bytes: Vec<u8>,
    pub public_outputs: Vec<[u8; 32]>,
    pub compute_time_ms: u64,
}

impl ProofSubmission {
    pub fn new(
        request_id: [u8; 32],
        prover: [u8; 32],
        proof_bytes: Vec<u8>,
        public_outputs: Vec<[u8; 32]>,
        compute_time_ms: u64,
    ) -> Self {
        Self {
            request_id,
            prover,
            proof_bytes,
            public_outputs,
            compute_time_ms,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofVerificationResult {
    pub request_id: [u8; 32],
    pub verified: bool,
    pub verifier: [u8; 32],
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProverStats {
    pub address: [u8; 32],
    pub total_proofs: u64,
    pub success_rate: f64,
    pub avg_time_ms: u64,
    pub total_earned: u64,
}

impl ProverStats {
    pub fn new(address: [u8; 32]) -> Self {
        Self {
            address,
            total_proofs: 0,
            success_rate: 0.0,
            avg_time_ms: 0,
            total_earned: 0,
        }
    }

    pub fn record_proof(&mut self, success: bool, time_ms: u64, earned: u64) {
        let was_total = self.total_proofs;
        self.total_proofs += 1;

        let successes = (self.success_rate * was_total as f64) + if success { 1.0 } else { 0.0 };
        self.success_rate = successes / self.total_proofs as f64;

        let total_time = self.avg_time_ms * was_total + time_ms;
        self.avg_time_ms = total_time / self.total_proofs;

        self.total_earned += earned;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RequestStatus {
    Open,
    Assigned,
    Fulfilled,
    #[allow(dead_code)]
    Expired,
}

#[derive(Debug, Clone)]
struct RequestEntry {
    request: ProofRequest,
    status: RequestStatus,
    offers: Vec<ProofOffer>,
    accepted_prover: Option<[u8; 32]>,
    submission: Option<ProofSubmission>,
    verification: Option<ProofVerificationResult>,
}

#[derive(Debug)]
pub struct ProofMarketplace {
    requests: HashMap<[u8; 32], RequestEntry>,
    prover_stats: HashMap<[u8; 32], ProverStats>,
    current_time: u64,
}

impl ProofMarketplace {
    pub fn new(current_time: u64) -> Self {
        Self {
            requests: HashMap::new(),
            prover_stats: HashMap::new(),
            current_time,
        }
    }

    pub fn set_time(&mut self, time: u64) {
        self.current_time = time;
    }

    pub fn submit_request(&mut self, request: ProofRequest) -> Result<(), ProofMarketError> {
        if request.is_expired(self.current_time) {
            return Err(ProofMarketError::DeadlinePassed);
        }
        let id = request.id;
        self.requests.insert(
            id,
            RequestEntry {
                request,
                status: RequestStatus::Open,
                offers: Vec::new(),
                accepted_prover: None,
                submission: None,
                verification: None,
            },
        );
        Ok(())
    }

    pub fn make_offer(&mut self, offer: ProofOffer) -> Result<(), ProofMarketError> {
        let entry = self
            .requests
            .get_mut(&offer.request_id)
            .ok_or(ProofMarketError::RequestNotFound)?;

        if entry.status != RequestStatus::Open {
            return Err(ProofMarketError::OfferAlreadyAccepted);
        }

        if entry.offers.iter().any(|o| o.prover == offer.prover) {
            return Err(ProofMarketError::DuplicateOffer);
        }

        if offer.price > entry.request.max_cost {
            return Err(ProofMarketError::InsufficientPayment);
        }

        self.prover_stats
            .entry(offer.prover)
            .or_insert_with(|| ProverStats::new(offer.prover));

        entry.offers.push(offer);
        Ok(())
    }

    pub fn accept_offer(
        &mut self,
        request_id: &[u8; 32],
        prover: &[u8; 32],
    ) -> Result<(), ProofMarketError> {
        let entry = self
            .requests
            .get_mut(request_id)
            .ok_or(ProofMarketError::RequestNotFound)?;

        if entry.status != RequestStatus::Open {
            return Err(ProofMarketError::OfferAlreadyAccepted);
        }

        if !entry.offers.iter().any(|o| &o.prover == prover) {
            return Err(ProofMarketError::OfferNotFound);
        }

        entry.status = RequestStatus::Assigned;
        entry.accepted_prover = Some(*prover);
        Ok(())
    }

    pub fn submit_proof(&mut self, submission: ProofSubmission) -> Result<(), ProofMarketError> {
        let entry = self
            .requests
            .get_mut(&submission.request_id)
            .ok_or(ProofMarketError::RequestNotFound)?;

        if entry.status == RequestStatus::Fulfilled {
            return Err(ProofMarketError::RequestAlreadyFulfilled);
        }
        if entry.status != RequestStatus::Assigned {
            return Err(ProofMarketError::OfferNotFound);
        }
        if entry.accepted_prover != Some(submission.prover) {
            return Err(ProofMarketError::ProverNotFound);
        }

        entry.submission = Some(submission);
        Ok(())
    }

    pub fn verify_proof(
        &mut self,
        request_id: &[u8; 32],
        verifier: [u8; 32],
        verified: bool,
    ) -> Result<ProofVerificationResult, ProofMarketError> {
        let entry = self
            .requests
            .get_mut(request_id)
            .ok_or(ProofMarketError::RequestNotFound)?;

        let submission = entry
            .submission
            .as_ref()
            .ok_or(ProofMarketError::InvalidProof)?;

        let result = ProofVerificationResult {
            request_id: *request_id,
            verified,
            verifier,
            timestamp: self.current_time,
        };

        let prover = submission.prover;
        let compute_time = submission.compute_time_ms;
        let earned = if verified {
            entry
                .offers
                .iter()
                .find(|o| o.prover == prover)
                .map(|o| o.price)
                .unwrap_or(0)
        } else {
            0
        };

        if let Some(stats) = self.prover_stats.get_mut(&prover) {
            stats.record_proof(verified, compute_time, earned);
        }

        if verified {
            entry.status = RequestStatus::Fulfilled;
        }
        entry.verification = Some(result.clone());

        Ok(result)
    }

    pub fn get_stats(&self, prover: &[u8; 32]) -> Result<&ProverStats, ProofMarketError> {
        self.prover_stats
            .get(prover)
            .ok_or(ProofMarketError::ProverNotFound)
    }

    pub fn open_request_count(&self) -> usize {
        self.requests
            .values()
            .filter(|e| e.status == RequestStatus::Open)
            .count()
    }

    pub fn fulfilled_count(&self) -> usize {
        self.requests
            .values()
            .filter(|e| e.status == RequestStatus::Fulfilled)
            .count()
    }

    pub fn prover_count(&self) -> usize {
        self.prover_stats.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_hash(n: u8) -> [u8; 32] {
        let mut h = [0u8; 32];
        h[0] = n;
        h
    }

    fn sample_request(id: u8) -> ProofRequest {
        ProofRequest::new(
            test_hash(id),
            test_hash(0xAA),
            test_hash(0xCC),
            vec![test_hash(1), test_hash(2)],
            1_000,
            1_000_000,
            ProofPriority::Standard,
        )
    }

    fn sample_offer(prover: u8, request_id: u8, price: u64) -> ProofOffer {
        ProofOffer::new(test_hash(prover), test_hash(request_id), 500, price, 0.95)
    }

    #[test]
    fn test_submit_request() {
        let mut mp = ProofMarketplace::new(100);
        let req = sample_request(1);
        assert!(mp.submit_request(req).is_ok());
        assert_eq!(mp.open_request_count(), 1);
    }

    #[test]
    fn test_reject_expired_request() {
        let mut mp = ProofMarketplace::new(2_000_000);
        let req = sample_request(1);
        let result = mp.submit_request(req);
        assert_eq!(result, Err(ProofMarketError::DeadlinePassed));
        assert_eq!(mp.open_request_count(), 0);
    }

    #[test]
    fn test_make_offer() {
        let mut mp = ProofMarketplace::new(100);
        mp.submit_request(sample_request(1)).unwrap();

        let offer = sample_offer(0x10, 1, 500);
        assert!(mp.make_offer(offer).is_ok());
        assert_eq!(mp.prover_count(), 1);
    }

    #[test]
    fn test_reject_duplicate_offer() {
        let mut mp = ProofMarketplace::new(100);
        mp.submit_request(sample_request(1)).unwrap();

        let offer1 = sample_offer(0x10, 1, 500);
        let offer2 = sample_offer(0x10, 1, 400);
        assert!(mp.make_offer(offer1).is_ok());
        assert_eq!(mp.make_offer(offer2), Err(ProofMarketError::DuplicateOffer));
    }

    #[test]
    fn test_reject_overpriced_offer() {
        let mut mp = ProofMarketplace::new(100);
        mp.submit_request(sample_request(1)).unwrap();

        let offer = sample_offer(0x10, 1, 5_000);
        assert_eq!(
            mp.make_offer(offer),
            Err(ProofMarketError::InsufficientPayment)
        );
    }

    #[test]
    fn test_accept_offer() {
        let mut mp = ProofMarketplace::new(100);
        mp.submit_request(sample_request(1)).unwrap();
        mp.make_offer(sample_offer(0x10, 1, 500)).unwrap();

        assert!(mp.accept_offer(&test_hash(1), &test_hash(0x10)).is_ok());
        assert_eq!(mp.open_request_count(), 0);
    }

    #[test]
    fn test_full_lifecycle() {
        let mut mp = ProofMarketplace::new(100);

        mp.submit_request(sample_request(1)).unwrap();

        mp.make_offer(sample_offer(0x10, 1, 500)).unwrap();

        mp.accept_offer(&test_hash(1), &test_hash(0x10)).unwrap();

        let submission = ProofSubmission::new(
            test_hash(1),
            test_hash(0x10),
            vec![0xDE, 0xAD, 0xBE, 0xEF],
            vec![test_hash(42)],
            300,
        );
        assert!(mp.submit_proof(submission).is_ok());

        let result = mp
            .verify_proof(&test_hash(1), test_hash(0xFF), true)
            .unwrap();
        assert!(result.verified);
        assert_eq!(result.verifier, test_hash(0xFF));
        assert_eq!(mp.fulfilled_count(), 1);

        let stats = mp.get_stats(&test_hash(0x10)).unwrap();
        assert_eq!(stats.total_proofs, 1);
        assert!((stats.success_rate - 1.0).abs() < f64::EPSILON);
        assert_eq!(stats.total_earned, 500);
    }

    #[test]
    fn test_failed_verification_updates_stats() {
        let mut mp = ProofMarketplace::new(100);
        mp.submit_request(sample_request(1)).unwrap();
        mp.make_offer(sample_offer(0x10, 1, 500)).unwrap();
        mp.accept_offer(&test_hash(1), &test_hash(0x10)).unwrap();

        let submission =
            ProofSubmission::new(test_hash(1), test_hash(0x10), vec![0x00], vec![], 200);
        mp.submit_proof(submission).unwrap();

        let result = mp
            .verify_proof(&test_hash(1), test_hash(0xFF), false)
            .unwrap();
        assert!(!result.verified);

        let stats = mp.get_stats(&test_hash(0x10)).unwrap();
        assert_eq!(stats.total_proofs, 1);
        assert!(stats.success_rate.abs() < f64::EPSILON);
        assert_eq!(stats.total_earned, 0);
    }

    #[test]
    fn test_prover_stats_rolling_averages() {
        let mut stats = ProverStats::new(test_hash(1));

        stats.record_proof(true, 100, 500);
        assert_eq!(stats.total_proofs, 1);
        assert!((stats.success_rate - 1.0).abs() < f64::EPSILON);
        assert_eq!(stats.avg_time_ms, 100);
        assert_eq!(stats.total_earned, 500);

        stats.record_proof(false, 200, 0);
        assert_eq!(stats.total_proofs, 2);
        assert!((stats.success_rate - 0.5).abs() < f64::EPSILON);
        assert_eq!(stats.avg_time_ms, 150);
        assert_eq!(stats.total_earned, 500);

        stats.record_proof(true, 300, 700);
        assert_eq!(stats.total_proofs, 3);
        assert!((stats.success_rate - 2.0 / 3.0).abs() < 1e-10);
        assert_eq!(stats.avg_time_ms, 200);
        assert_eq!(stats.total_earned, 1200);
    }

    #[test]
    fn test_priority_cost_multipliers() {
        assert!((ProofPriority::Economy.cost_multiplier() - 0.5).abs() < f64::EPSILON);
        assert!((ProofPriority::Standard.cost_multiplier() - 1.0).abs() < f64::EPSILON);
        assert!((ProofPriority::Express.cost_multiplier() - 2.0).abs() < f64::EPSILON);
        assert!((ProofPriority::Instant.cost_multiplier() - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_submit_proof_unknown_request() {
        let mut mp = ProofMarketplace::new(100);
        let submission =
            ProofSubmission::new(test_hash(99), test_hash(0x10), vec![0x00], vec![], 100);
        assert_eq!(
            mp.submit_proof(submission),
            Err(ProofMarketError::RequestNotFound)
        );
    }

    #[test]
    fn test_accept_nonexistent_offer() {
        let mut mp = ProofMarketplace::new(100);
        mp.submit_request(sample_request(1)).unwrap();
        mp.make_offer(sample_offer(0x10, 1, 500)).unwrap();

        let result = mp.accept_offer(&test_hash(1), &test_hash(0x20));
        assert_eq!(result, Err(ProofMarketError::OfferNotFound));
    }

    #[test]
    fn test_proof_request_serde() {
        let req = sample_request(1);
        let json = serde_json::to_string(&req).expect("serialize ProofRequest");
        let back: ProofRequest = serde_json::from_str(&json).expect("deserialize ProofRequest");
        assert_eq!(back.id, req.id);
        assert_eq!(back.requester, req.requester);
        assert_eq!(back.circuit_id, req.circuit_id);
        assert_eq!(back.max_cost, req.max_cost);
        assert_eq!(back.deadline, req.deadline);
    }
}

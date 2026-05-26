//! # GigGuard — Milestone Escrow with 72-Hour Auto-Release
//!
//! A Soroban smart contract that protects freelancers from non-payment.
//! Clients lock USDC upfront, freelancers deliver per milestone, and funds
//! auto-release after 72 hours if the client doesn't respond.
//!
//! ## Core Flow
//! 1. Client creates a job with milestones and deposits USDC
//! 2. Freelancer submits completed milestones
//! 3. Client approves → funds release instantly
//! 4. Client ghosts → funds auto-release after 72 hours
//! 5. Client disputes → milestone is frozen for resolution
#![no_std]
use soroban_sdk::{contract, contractimpl, contracttype, token, Address, Env, String, Vec};
// ============================================================================
// DATA TYPES
// ============================================================================
/// Tracks the lifecycle of each milestone through the escrow process.
/// Transitions: Pending → Submitted → Approved/Disputed/Released
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MilestoneStatus {
    /// Milestone created, waiting for freelancer to deliver work
    Pending,
    /// Freelancer has submitted deliverables, awaiting client review
    Submitted,
    /// Client approved the milestone, funds have been released
    Approved,
    /// Client raised a dispute, milestone is frozen
    Disputed,
    /// Funds auto-released via 72-hour timeout (client didn't respond)
    Released,
}
/// Represents a single milestone within a job.
/// Each milestone has its own budget, status, and submission timestamp.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    /// Human-readable description of the deliverable (e.g., "Homepage design")
    pub description: String,
    /// Amount in token's smallest unit (e.g., 250_0000000 = $250 USDC with 7 decimals)
    pub amount: i128,
    /// Current lifecycle status of this milestone
    pub status: MilestoneStatus,
    /// Ledger timestamp when freelancer submitted — used for 72-hour timeout calc
    pub submitted_at: u64,
}
/// Represents a complete freelance job/contract between a client and freelancer.
/// Contains all milestones and tracks the overall escrow state.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Job {
    /// The client who created and funded the job
    pub client: Address,
    /// The freelancer who will deliver the work
    pub freelancer: Address,
    /// The token contract address used for payment (e.g., USDC on Stellar)
    pub token: Address,
    /// Total amount deposited across all milestones
    pub total_amount: i128,
    /// List of milestones with individual amounts and statuses
    pub milestones: Vec<Milestone>,
    /// Whether this job is still active (not cancelled or fully completed)
    pub is_active: bool,
    /// Platform fee in basis points (50 = 0.5%)
    pub fee_bps: u32,
}
/// Storage keys for the contract's persistent and instance data.
#[contracttype]
pub enum DataKey {
    /// Stores a Job struct, keyed by job ID
    Job(u64),
    /// Counter for auto-incrementing job IDs
    JobCount,
    /// Address that receives platform fees
    FeeCollector,
}
// ============================================================================
// CONSTANTS
// ============================================================================
/// 72 hours in seconds (72 * 60 * 60 = 259,200)
/// If a client doesn't approve or dispute within this window after
/// freelancer submission, the freelancer can claim funds automatically.
const TIMEOUT_SECONDS: u64 = 259_200;
/// Platform fee: 50 basis points = 0.5%
/// Deducted from each milestone payout. Much lower than traditional
/// escrow services which charge 8–15%.
const FEE_BPS: u32 = 50;
// ============================================================================
// CONTRACT
// ============================================================================
#[contract]
pub struct GigGuardContract;
#[contractimpl]
impl GigGuardContract {
    // ========================================================================
    // INITIALIZATION
    // ========================================================================
    /// Sets up the contract with a fee collector address and initializes
    /// the job counter to zero. Must be called once after deployment.
    ///
    /// # Arguments
    /// * `fee_collector` - Address that will receive the 0.5% platform fees
    pub fn initialize(env: Env, fee_collector: Address) {
        // Store the fee collector address — this is where platform fees are sent
        env.storage()
            .instance()
            .set(&DataKey::FeeCollector, &fee_collector);
        // Initialize job counter at 0 — auto-increments with each new job
        env.storage().instance().set(&DataKey::JobCount, &0u64);
    }
    // ========================================================================
    // JOB CREATION — Client deposits funds and defines milestones
    // ========================================================================
    /// Creates a new escrow job. The client defines milestones with amounts
    /// and descriptions, then deposits the TOTAL amount into the contract.
    /// Funds are locked on-chain — the client cannot withdraw them.
    ///
    /// # Arguments
    /// * `client` - The client's address (must authorize the transaction)
    /// * `freelancer` - The freelancer's address who will do the work
    /// * `token` - The payment token contract address (e.g., USDC)
    /// * `milestone_amounts` - Payment amount for each milestone
    /// * `milestone_descriptions` - Description of each milestone's deliverable
    ///
    /// # Returns
    /// * `u64` - The unique job ID
    ///
    /// # Why Stellar?
    /// The token transfer uses Stellar's native USDC — no bridging, no wrapping.
    /// Transfer costs <$0.01 vs $5–20 on PayPal/Escrow.com.
    pub fn create_job(
        env: Env,
        client: Address,
        freelancer: Address,
        token: Address,
        milestone_amounts: Vec<i128>,
        milestone_descriptions: Vec<String>,
    ) -> u64 {
        // Client must authorize — proves they consent to locking their funds
        client.require_auth();
        // Validate: amounts and descriptions must match 1:1
        assert!(
            milestone_amounts.len() == milestone_descriptions.len(),
            "Mismatched milestone data"
        );
        assert!(milestone_amounts.len() > 0, "Must have at least one milestone");
        // Calculate total amount across all milestones
        let mut total: i128 = 0;
        for i in 0..milestone_amounts.len() {
            let amt = milestone_amounts.get(i).unwrap();
            assert!(amt > 0, "Milestone amount must be positive");
            total += amt;
        }
        // Transfer the total amount from client to the contract address.
        // After this, funds are LOCKED — the client cannot retrieve them.
        // This is the key anti-ghosting mechanism: money is committed upfront.
        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&client, &env.current_contract_address(), &total);
        // Build the milestone structs with initial "Pending" status
        let mut milestones = Vec::new(&env);
        for i in 0..milestone_amounts.len() {
            milestones.push_back(Milestone {
                description: milestone_descriptions.get(i).unwrap(),
                amount: milestone_amounts.get(i).unwrap(),
                status: MilestoneStatus::Pending,
                submitted_at: 0, // Will be set when freelancer submits
            });
        }
        // Get next job ID (auto-increment)
        let job_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::JobCount)
            .unwrap_or(0);
        // Construct and store the job
        let job = Job {
            client,
            freelancer,
            token,
            total_amount: total,
            milestones,
            is_active: true,
            fee_bps: FEE_BPS,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Job(job_id), &job);
        // Increment job counter for next creation
        env.storage()
            .instance()
            .set(&DataKey::JobCount, &(job_id + 1));
        job_id
    }
    // ========================================================================
    // MILESTONE SUBMISSION — Freelancer delivers work
    // ========================================================================
    /// Freelancer marks a milestone as submitted after delivering work.
    /// This starts the 72-hour countdown for the client to respond.
    ///
    /// # Arguments
    /// * `freelancer` - Must match the job's freelancer address
    /// * `job_id` - The job to update
    /// * `milestone_index` - Which milestone was completed (0-indexed)
    ///
    /// # Important
    /// The `submitted_at` timestamp is recorded from the ledger — this is
    /// the clock that starts the 72-hour auto-release countdown.
    pub fn submit_milestone(env: Env, freelancer: Address, job_id: u64, milestone_index: u32) {
        // Only the assigned freelancer can submit
        freelancer.require_auth();
        let mut job: Job = env
            .storage()
            .persistent()
            .get(&DataKey::Job(job_id))
            .expect("Job not found");
        assert!(job.is_active, "Job is not active");
        assert!(
            job.freelancer == freelancer,
            "Not the assigned freelancer for this job"
        );
        let mut milestone = job.milestones.get(milestone_index).expect("Invalid milestone index");
        assert!(
            milestone.status == MilestoneStatus::Pending,
            "Milestone is not in pending state"
        );
        // Mark as submitted and record the timestamp
        // This timestamp is critical — it's used to calculate the 72-hour timeout
        milestone.status = MilestoneStatus::Submitted;
        milestone.submitted_at = env.ledger().timestamp();
        job.milestones.set(milestone_index, milestone);
        env.storage()
            .persistent()
            .set(&DataKey::Job(job_id), &job);
    }
    // ========================================================================
    // MILESTONE APPROVAL — Client approves and funds release
    // ========================================================================
    /// Client approves a submitted milestone, releasing funds to the freelancer.
    /// A 0.5% platform fee is deducted and sent to the fee collector.
    ///
    /// # Arguments
    /// * `client` - Must match the job's client address
    /// * `job_id` - The job to update
    /// * `milestone_index` - Which milestone to approve (0-indexed)
    ///
    /// # On-chain effect
    /// Two token transfers occur:
    /// 1. (amount - fee) → freelancer wallet (instant, <5 seconds on Stellar)
    /// 2. fee → fee collector address
    pub fn approve_milestone(env: Env, client: Address, job_id: u64, milestone_index: u32) {
        // Only the job's client can approve
        client.require_auth();
        let mut job: Job = env
            .storage()
            .persistent()
            .get(&DataKey::Job(job_id))
            .expect("Job not found");
        assert!(job.is_active, "Job is not active");
        assert!(
            job.client == client,
            "Not the client for this job"
        );
        let mut milestone = job.milestones.get(milestone_index).expect("Invalid milestone index");
        assert!(
            milestone.status == MilestoneStatus::Submitted,
            "Milestone has not been submitted yet"
        );
        // Calculate platform fee (0.5%) and freelancer payout
        // For $250 USDC: fee = $1.25, payout = $248.75
        let fee = (milestone.amount * job.fee_bps as i128) / 10_000;
        let payout = milestone.amount - fee;
        let token_client = token::Client::new(&env, &job.token);
        // Transfer payout to freelancer — settles in ~5 seconds on Stellar
        token_client.transfer(&env.current_contract_address(), &job.freelancer, &payout);
        // Transfer platform fee to fee collector
        if fee > 0 {
            let fee_collector: Address = env
                .storage()
                .instance()
                .get(&DataKey::FeeCollector)
                .expect("Fee collector not set");
            token_client.transfer(&env.current_contract_address(), &fee_collector, &fee);
        }
        // Update milestone status to Approved
        milestone.status = MilestoneStatus::Approved;
        job.milestones.set(milestone_index, milestone);
        env.storage()
            .persistent()
            .set(&DataKey::Job(job_id), &job);
    }
    // ========================================================================
    // TIMEOUT CLAIM — The anti-ghosting mechanism 👻❌
    // ========================================================================
    /// Freelancer claims funds after the 72-hour timeout has passed.
    /// This is the KILLER FEATURE — if the client disappears after delivery,
    /// the freelancer is guaranteed payment. No human intervention needed.
    ///
    /// # Arguments
    /// * `freelancer` - Must match the job's freelancer address
    /// * `job_id` - The job to claim from
    /// * `milestone_index` - Which milestone to claim (0-indexed)
    ///
    /// # Why this only works on blockchain
    /// A traditional escrow service requires a human to decide. Here, the
    /// smart contract checks the clock and enforces the rule automatically.
    /// No customer support tickets. No waiting. Just math.
    pub fn claim_timeout(env: Env, freelancer: Address, job_id: u64, milestone_index: u32) {
        // Only the assigned freelancer can claim
        freelancer.require_auth();
        let mut job: Job = env
            .storage()
            .persistent()
            .get(&DataKey::Job(job_id))
            .expect("Job not found");
        assert!(job.is_active, "Job is not active");
        assert!(
            job.freelancer == freelancer,
            "Not the assigned freelancer for this job"
        );
        let mut milestone = job.milestones.get(milestone_index).expect("Invalid milestone index");
        // Can only timeout-claim milestones that are in "Submitted" state.
        // Disputed milestones are frozen and cannot be auto-claimed.
        assert!(
            milestone.status == MilestoneStatus::Submitted,
            "Milestone must be in submitted state to claim timeout"
        );
        // THE 72-HOUR CHECK: Has enough time passed since submission?
        let current_time = env.ledger().timestamp();
        assert!(
            current_time >= milestone.submitted_at + TIMEOUT_SECONDS,
            "72-hour timeout has not been reached yet"
        );
        // Calculate fee and payout (same as approval)
        let fee = (milestone.amount * job.fee_bps as i128) / 10_000;
        let payout = milestone.amount - fee;
        let token_client = token::Client::new(&env, &job.token);
        // Transfer payout to freelancer
        token_client.transfer(&env.current_contract_address(), &job.freelancer, &payout);
        // Transfer platform fee
        if fee > 0 {
            let fee_collector: Address = env
                .storage()
                .instance()
                .get(&DataKey::FeeCollector)
                .expect("Fee collector not set");
            token_client.transfer(&env.current_contract_address(), &fee_collector, &fee);
        }
        // Mark as Released (via timeout, not approval)
        milestone.status = MilestoneStatus::Released;
        job.milestones.set(milestone_index, milestone);
        env.storage()
            .persistent()
            .set(&DataKey::Job(job_id), &job);
    }
    // ========================================================================
    // DISPUTE — Client flags a problem
    // ========================================================================
    /// Client disputes a submitted milestone, freezing the funds.
    /// This prevents the 72-hour auto-release from triggering.
    /// Dispute resolution happens off-chain (for MVP); in production,
    /// a DAO or arbitrator contract could resolve disputes on-chain.
    ///
    /// # Arguments
    /// * `client` - Must match the job's client address
    /// * `job_id` - The job to dispute
    /// * `milestone_index` - Which milestone to dispute (0-indexed)
    pub fn dispute_milestone(env: Env, client: Address, job_id: u64, milestone_index: u32) {
        // Only the job's client can raise a dispute
        client.require_auth();
        let mut job: Job = env
            .storage()
            .persistent()
            .get(&DataKey::Job(job_id))
            .expect("Job not found");
        assert!(job.is_active, "Job is not active");
        assert!(
            job.client == client,
            "Not the client for this job"
        );
        let mut milestone = job.milestones.get(milestone_index).expect("Invalid milestone index");
        // Can only dispute milestones that have been submitted
        assert!(
            milestone.status == MilestoneStatus::Submitted,
            "Can only dispute submitted milestones"
        );
        // Freeze the milestone — blocks auto-release via timeout
        milestone.status = MilestoneStatus::Disputed;
        job.milestones.set(milestone_index, milestone);
        env.storage()
            .persistent()
            .set(&DataKey::Job(job_id), &job);
    }
    // ========================================================================
    // VIEW FUNCTIONS — Read-only queries
    // ========================================================================
    /// Returns the full job details including all milestones and their statuses.
    /// Used by the frontend to display job progress to both parties.
    pub fn get_job(env: Env, job_id: u64) -> Job {
        env.storage()
            .persistent()
            .get(&DataKey::Job(job_id))
            .expect("Job not found")
    }
    /// Returns the total number of jobs created on the contract.
    /// Useful for frontend pagination and analytics.
    pub fn get_job_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::JobCount)
            .unwrap_or(0)
    }
}
// Include test module when running `cargo test`
#[cfg(test)]
mod test;
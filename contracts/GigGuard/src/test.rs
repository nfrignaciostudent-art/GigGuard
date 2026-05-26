//! # GigGuard — Test Suite
//!
//! Exactly 5 tests covering the core MVP transaction flow:
//! 1. Happy path: full end-to-end milestone approval
//! 2. Edge case: unauthorized caller tries to approve
//! 3. State verification: storage correctness after job creation
//! 4. Timeout auto-release: freelancer claims after 72 hours
//! 5. Dispute blocks timeout: disputed milestone cannot be auto-claimed
use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token, vec, Env, String,
};
// ============================================================================
// HELPER: Sets up a clean test environment with token, addresses, and contract
// ============================================================================
/// Creates a fully initialized test environment with:
/// - GigGuard contract deployed and initialized
/// - A test USDC token with the client funded (10,000 USDC)
/// - Separate addresses for client, freelancer, and fee collector
struct TestSetup<'a> {
    env: Env,
    gig_client: GigGuardContractClient<'a>,
    token_addr: Address,
    client: Address,
    freelancer: Address,
    fee_collector: Address,
}
fn setup() -> TestSetup<'static> {
    let env = Env::default();
    // Mock all authorization checks so tests can call functions freely.
    // In production, Stellar's native auth handles this.
    env.mock_all_auths();
    // Deploy the GigGuard contract
    let contract_id = env.register(GigGuardContract, ());
    let gig_client = GigGuardContractClient::new(&env, &contract_id);
    // Create test addresses
    let fee_collector = Address::generate(&env);
    let client = Address::generate(&env);
    let freelancer = Address::generate(&env);
    // Create a test USDC-like token (Stellar Asset Contract)
    let token_admin = Address::generate(&env);
    let token_contract = env.register_stellar_asset_contract_v2(token_admin.clone());
    let token_addr = token_contract.address().clone();
    let token_admin_client = token::StellarAssetClient::new(&env, &token_addr);
    // Mint 10,000 USDC (with 7 decimal places) to the client
    // 10_000 * 10^7 = 100_000_000_000
    token_admin_client.mint(&client, &100_000_000_000i128);
    // Initialize GigGuard with fee collector
    gig_client.initialize(&fee_collector);
    TestSetup {
        env,
        gig_client,
        token_addr,
        client,
        freelancer,
        fee_collector,
    }
}
/// Helper: creates a standard 2-milestone job ($250 + $250 = $500 USDC)
fn create_standard_job(setup: &TestSetup) -> u64 {
    let amounts = vec![
        &setup.env,
        2_500_000_000i128, // $250 USDC (7 decimals)
        2_500_000_000i128, // $250 USDC
    ];
    let descriptions = vec![
        &setup.env,
        String::from_str(&setup.env, "Homepage design"),
        String::from_str(&setup.env, "Full website build"),
    ];
    setup.gig_client.create_job(
        &setup.client,
        &setup.freelancer,
        &setup.token_addr,
        &amounts,
        &descriptions,
    )
}
// ============================================================================
// TEST 1 — HAPPY PATH: Full MVP Transaction End-to-End
// ============================================================================
// Scenario: Client creates a $500 job (2 milestones × $250).
// Freelancer submits milestone 1. Client approves. Freelancer gets paid.
// This proves the core product works in under 2 minutes.
#[test]
fn test_happy_path_create_submit_approve() {
    let setup = setup();
    let token = token::Client::new(&setup.env, &setup.token_addr);
    // Step 1: Client creates job and deposits $500 USDC
    let job_id = create_standard_job(&setup);
    // Step 2: Freelancer submits milestone 0 (Homepage design)
    setup
        .gig_client
        .submit_milestone(&setup.freelancer, &job_id, &0u32);
    // Step 3: Client approves milestone 0 → $250 releases to freelancer
    setup
        .gig_client
        .approve_milestone(&setup.client, &job_id, &0u32);
    // Verify: Freelancer received $250 minus 0.5% fee
    // Fee: $250 * 0.005 = $1.25 = 12_500_000 (7 decimals)
    // Payout: $250 - $1.25 = $248.75 = 2_487_500_000
    let freelancer_balance = token.balance(&setup.freelancer);
    assert_eq!(freelancer_balance, 2_487_500_000i128);
    // Verify: Fee collector received the $1.25 fee
    let fee_balance = token.balance(&setup.fee_collector);
    assert_eq!(fee_balance, 12_500_000i128);
    // Verify: Milestone status updated to Approved
    let job = setup.gig_client.get_job(&job_id);
    assert_eq!(
        job.milestones.get(0).unwrap().status,
        MilestoneStatus::Approved
    );
}
// ============================================================================
// TEST 2 — EDGE CASE: Unauthorized Caller Tries to Approve
// ============================================================================
// Scenario: A random address (not the client) tries to approve a milestone.
// The contract MUST reject this — only the job's client can release funds.
#[test]
#[should_panic(expected = "Not the client for this job")]
fn test_unauthorized_approve_rejected() {
    let setup = setup();
    // Create job where `setup.client` is the actual client
    let job_id = create_standard_job(&setup);
    // Freelancer submits milestone 0
    setup
        .gig_client
        .submit_milestone(&setup.freelancer, &job_id, &0u32);
    // A random impostor tries to approve — this MUST panic
    let impostor = Address::generate(&setup.env);
    setup
        .gig_client
        .approve_milestone(&impostor, &job_id, &0u32);
}
// ============================================================================
// TEST 3 — STATE VERIFICATION: Storage Correctness After Job Creation
// ============================================================================
// Scenario: After creating a job, verify every field in contract storage
// is correct — amounts, addresses, milestone count, statuses, activity flag.
#[test]
fn test_state_correct_after_job_creation() {
    let setup = setup();
    let token = token::Client::new(&setup.env, &setup.token_addr);
    // Create the standard 2-milestone job
    let job_id = create_standard_job(&setup);
    // Retrieve job from contract storage
    let job = setup.gig_client.get_job(&job_id);
    // Verify all job fields
    assert_eq!(job.client, setup.client);
    assert_eq!(job.freelancer, setup.freelancer);
    assert_eq!(job.token, setup.token_addr);
    assert_eq!(job.total_amount, 5_000_000_000i128); // $500 total
    assert_eq!(job.is_active, true);
    assert_eq!(job.fee_bps, 50); // 0.5%
    // Verify milestones
    assert_eq!(job.milestones.len(), 2);
    let m0 = job.milestones.get(0).unwrap();
    assert_eq!(m0.amount, 2_500_000_000i128); // $250
    assert_eq!(m0.status, MilestoneStatus::Pending);
    assert_eq!(m0.submitted_at, 0); // Not yet submitted
    let m1 = job.milestones.get(1).unwrap();
    assert_eq!(m1.amount, 2_500_000_000i128); // $250
    assert_eq!(m1.status, MilestoneStatus::Pending);
    // Verify funds transferred from client to contract
    // Client started with 100_000_000_000 and deposited 5_000_000_000
    let client_balance = token.balance(&setup.client);
    assert_eq!(client_balance, 95_000_000_000i128);
    // Verify job counter incremented
    let count = setup.gig_client.get_job_count();
    assert_eq!(count, 1);
}
// ============================================================================
// TEST 4 — TIMEOUT AUTO-RELEASE: Freelancer Claims After 72 Hours
// ============================================================================
// Scenario: Freelancer submits milestone. Client ghosts (doesn't respond).
// After 72 hours pass, freelancer calls `claim_timeout` and gets paid.
// THIS IS THE KILLER FEATURE — proves ghosting is impossible.
#[test]
fn test_timeout_auto_release_after_72_hours() {
    let setup = setup();
    let token = token::Client::new(&setup.env, &setup.token_addr);
    // Set initial ledger timestamp to a known value
    setup.env.ledger().with_mut(|info| {
        info.timestamp = 1_000_000; // Arbitrary start time
    });
    // Create job and freelancer submits milestone 0
    let job_id = create_standard_job(&setup);
    setup
        .gig_client
        .submit_milestone(&setup.freelancer, &job_id, &0u32);
    // Verify submission timestamp was recorded
    let job = setup.gig_client.get_job(&job_id);
    assert_eq!(job.milestones.get(0).unwrap().submitted_at, 1_000_000);
    // Fast-forward time by 72 hours + 1 second (259,201 seconds)
    // Simulating the client ghosting for 3 days
    setup.env.ledger().with_mut(|info| {
        info.timestamp = 1_000_000 + 259_201; // Past the 72-hour window
    });
    // Freelancer claims via timeout — should succeed
    setup
        .gig_client
        .claim_timeout(&setup.freelancer, &job_id, &0u32);
    // Verify: Freelancer received $248.75 ($250 - 0.5% fee)
    let freelancer_balance = token.balance(&setup.freelancer);
    assert_eq!(freelancer_balance, 2_487_500_000i128);
    // Verify: Milestone status is "Released" (not "Approved")
    let job = setup.gig_client.get_job(&job_id);
    assert_eq!(
        job.milestones.get(0).unwrap().status,
        MilestoneStatus::Released
    );
}
// ============================================================================
// TEST 5 — DISPUTE BLOCKS TIMEOUT: Client Disputes Before 72 Hours
// ============================================================================
// Scenario: Freelancer submits, client disputes within 72 hours.
// The milestone is frozen — freelancer CANNOT auto-claim via timeout.
// This protects clients from fraudulent submissions.
#[test]
#[should_panic(expected = "Milestone must be in submitted state to claim timeout")]
fn test_dispute_blocks_timeout_claim() {
    let setup = setup();
    // Set initial timestamp
    setup.env.ledger().with_mut(|info| {
        info.timestamp = 1_000_000;
    });
    // Create job and freelancer submits milestone 0
    let job_id = create_standard_job(&setup);
    setup
        .gig_client
        .submit_milestone(&setup.freelancer, &job_id, &0u32);
    // Client disputes the milestone (within 72 hours)
    setup
        .gig_client
        .dispute_milestone(&setup.client, &job_id, &0u32);
    // Verify milestone is now Disputed
    let job = setup.gig_client.get_job(&job_id);
    assert_eq!(
        job.milestones.get(0).unwrap().status,
        MilestoneStatus::Disputed
    );
    // Fast-forward past 72 hours
    setup.env.ledger().with_mut(|info| {
        info.timestamp = 1_000_000 + 300_000; // Well past 72 hours
    });
    // Freelancer tries to claim timeout — MUST PANIC because milestone is Disputed, not Submitted
    setup
        .gig_client
        .claim_timeout(&setup.freelancer, &job_id, &0u32);
}

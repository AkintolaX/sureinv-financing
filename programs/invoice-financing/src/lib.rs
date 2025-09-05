use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Mint, Transfer};

declare_id!("Fg6PaFpoGXkYsidMpWTK6W2BeZ7FEfcYkg476zPFsLnS");

#[program]
pub mod invoice_financing {
    use super::*;

    // Initialize the global program state
    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        let global_state = &mut ctx.accounts.global_state;
        global_state.total_invoices = 0;
        global_state.total_funded = 0;
        global_state.insurance_pool_balance = 0;
        global_state.authority = ctx.accounts.authority.key();
        global_state.usdc_mint = ctx.accounts.usdc_mint.key();
        global_state.bump = ctx.bumps.global_state;
        
        msg!("Global state initialized with authority: {}", global_state.authority);
        Ok(())
    }

    // Create a new invoice for financing
    pub fn create_invoice(
        ctx: Context<CreateInvoice>,
        invoice_id: u64,
        amount: u64,
        due_date: i64,
        debtor_info: String,
    ) -> Result<()> {
        let invoice = &mut ctx.accounts.invoice;
        let global_state = &mut ctx.accounts.global_state;

        // Comprehensive validation
        require!(amount > 0, ErrorCode::InvalidAmount);
        require!(amount <= 10_000_000_000, ErrorCode::AmountTooLarge); // Max 10k USDC
        require!(due_date > Clock::get()?.unix_timestamp, ErrorCode::InvalidDueDate);
        require!(due_date <= Clock::get()?.unix_timestamp + 365 * 24 * 3600, ErrorCode::DueDateTooFar); // Max 1 year
        require!(debtor_info.len() <= 200, ErrorCode::DebtorInfoTooLong);
        require!(debtor_info.len() >= 10, ErrorCode::DebtorInfoTooShort);

        // Enhanced risk calculation
        let risk_assessment = calculate_enhanced_risk(
            amount,
            due_date,
            &ctx.accounts.business_owner.key(),
            &global_state
        )?;
        
        // Calculate insurance premium based on risk
        let insurance_premium = (amount * risk_assessment.risk_score as u64) / 1000;

        // Set invoice data
        invoice.invoice_id = invoice_id;
        invoice.business_owner = ctx.accounts.business_owner.key();
        invoice.amount = amount;
        invoice.due_date = due_date;
        invoice.debtor_info = debtor_info;
        invoice.status = InvoiceStatus::PendingFunding;
        invoice.risk_score = risk_assessment.risk_score;
        invoice.insurance_premium = insurance_premium;
        invoice.created_at = Clock::get()?.unix_timestamp;
        invoice.funded_amount = 0;
        invoice.investor = Pubkey::default();
        invoice.bump = ctx.bumps.invoice;

        // Additional risk factors
        invoice.industry_risk = risk_assessment.industry_risk;
        invoice.credit_score = risk_assessment.estimated_credit_score;
        invoice.payment_terms_days = ((due_date - Clock::get()?.unix_timestamp) / 86400) as u16;

        // Update global state
        global_state.total_invoices += 1;

        emit!(InvoiceCreated {
            invoice_id,
            business_owner: ctx.accounts.business_owner.key(),
            amount,
            risk_score: risk_assessment.risk_score,
            insurance_premium,
            estimated_yield: risk_assessment.estimated_yield,
        });

        msg!("Invoice {} created successfully with risk score: {}", invoice_id, risk_assessment.risk_score);
        Ok(())
    }

    // Fund an invoice (investor provides capital)
    pub fn fund_invoice(
        ctx: Context<FundInvoice>,
        amount: u64,
    ) -> Result<()> {
        let invoice = &mut ctx.accounts.invoice;
        let global_state = &mut ctx.accounts.global_state;

        // Enhanced validation
        require!(invoice.status == InvoiceStatus::PendingFunding, ErrorCode::InvoiceNotAvailable);
        require!(amount == invoice.amount, ErrorCode::InvalidFundingAmount); // Must fund full amount
        require!(
            ctx.accounts.investor_token_account.amount >= amount + invoice.insurance_premium,
            ErrorCode::InsufficientFunds
        );

        // Calculate total cost (principal + insurance premium)
        let _total_cost = amount + invoice.insurance_premium;

        // Transfer principal from investor to business owner
        let transfer_principal_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.investor_token_account.to_account_info(),
                to: ctx.accounts.business_token_account.to_account_info(),
                authority: ctx.accounts.investor.to_account_info(),
            },
        );
        token::transfer(transfer_principal_ctx, amount)?;

        // Transfer insurance premium to insurance pool
        let transfer_premium_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.investor_token_account.to_account_info(),
                to: ctx.accounts.insurance_pool_account.to_account_info(),
                authority: ctx.accounts.investor.to_account_info(),
            },
        );
        token::transfer(transfer_premium_ctx, invoice.insurance_premium)?;

        // Update invoice state
        invoice.status = InvoiceStatus::Funded;
        invoice.funded_amount = amount;
        invoice.investor = ctx.accounts.investor.key();
        invoice.funding_date = Some(Clock::get()?.unix_timestamp);

        // Calculate expected return (risk-based yield)
        let expected_return = amount + ((amount * invoice.risk_score as u64) / 500); // 2x risk score as APR
        invoice.expected_return = Some(expected_return);

        // Update global state
        global_state.total_funded += amount;
        global_state.insurance_pool_balance += invoice.insurance_premium;

        emit!(InvoiceFunded {
            invoice_id: invoice.invoice_id,
            investor: ctx.accounts.investor.key(),
            amount,
            insurance_premium: invoice.insurance_premium,
            expected_return,
        });

        msg!("Invoice {} funded by {} for {} USDC", invoice.invoice_id, ctx.accounts.investor.key(), amount);
        Ok(())
    }

    // Repay invoice when debtor pays
    pub fn repay_invoice(ctx: Context<RepayInvoice>, repayment_amount: u64) -> Result<()> {
        let invoice = &mut ctx.accounts.invoice;

        require!(invoice.status == InvoiceStatus::Funded, ErrorCode::InvoiceNotFunded);
        require!(repayment_amount >= invoice.funded_amount, ErrorCode::InsufficientRepayment);

        // Allow repayment up to 30 days after due date (grace period)
        let grace_period = 30 * 86400; // 30 days
        let current_time = Clock::get()?.unix_timestamp;
        
        // Check if within grace period
        let is_late = current_time > invoice.due_date;
        let days_overdue = if is_late {
            (current_time - invoice.due_date) / 86400
        } else {
            0
        };

        require!(
            current_time <= invoice.due_date + grace_period,
            ErrorCode::RepaymentPeriodExpired
        );

        // Calculate late fees if applicable
        let mut total_repayment = repayment_amount;
        let mut late_fee = 0u64;
        
        if is_late {
            late_fee = (invoice.funded_amount * days_overdue as u64 * 5) / 10000; // 0.05% per day
            total_repayment += late_fee;
        }

        require!(
            ctx.accounts.business_token_account.amount >= total_repayment,
            ErrorCode::InsufficientRepaymentFunds
        );

        // Transfer repayment from business owner to investor
        let transfer_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.business_token_account.to_account_info(),
                to: ctx.accounts.investor_token_account.to_account_info(),
                authority: ctx.accounts.business_owner.to_account_info(),
            },
        );
        token::transfer(transfer_ctx, total_repayment)?;

        invoice.status = InvoiceStatus::Repaid;
        invoice.repayment_date = Some(current_time);
        invoice.final_repayment_amount = Some(total_repayment);
        invoice.late_fee = Some(late_fee);

        emit!(InvoiceRepaid {
            invoice_id: invoice.invoice_id,
            amount: total_repayment,
            late_fee,
            days_overdue: days_overdue as u16,
        });

        msg!("Invoice {} repaid: {} USDC (late fee: {})", invoice.invoice_id, total_repayment, late_fee);
        Ok(())
    }

    // Claim insurance if invoice defaults
    pub fn claim_insurance(ctx: Context<ClaimInsurance>) -> Result<()> {
        let invoice = &mut ctx.accounts.invoice;
        let global_state = &mut ctx.accounts.global_state;

        require!(invoice.status == InvoiceStatus::Funded, ErrorCode::InvoiceNotFunded);
        require!(
            ctx.accounts.investor.key() == invoice.investor,
            ErrorCode::UnauthorizedInsuranceClaim
        );

        // Must wait 30 days after due date to claim
        let claim_eligible_date = invoice.due_date + (30 * 86400);
        require!(
            Clock::get()?.unix_timestamp > claim_eligible_date,
            ErrorCode::NotEligibleForClaim
        );

        // Calculate insurance payout based on risk tier
        let coverage_percentage = match invoice.risk_score {
            0..=20 => 90,   // Low risk: 90% coverage
            21..=35 => 80,  // Medium risk: 80% coverage
            36..=50 => 70,  // High risk: 70% coverage
            _ => 60,        // Very high risk: 60% coverage
        };

        let insurance_payout = (invoice.funded_amount * coverage_percentage) / 100;
        
        // Ensure insurance pool has sufficient funds
        require!(
            ctx.accounts.insurance_pool_account.amount >= insurance_payout,
            ErrorCode::InsufficientInsurancePool
        );

        // Transfer insurance payout to investor
        let seeds = &[b"insurance_pool".as_ref(), &[global_state.bump]];
        let signer_seeds = &[&seeds[..]];

        let transfer_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.insurance_pool_account.to_account_info(),
                to: ctx.accounts.investor_token_account.to_account_info(),
                authority: ctx.accounts.insurance_pool_authority.to_account_info(),
            },
            signer_seeds,
        );
        token::transfer(transfer_ctx, insurance_payout)?;

        invoice.status = InvoiceStatus::Defaulted;
        invoice.insurance_claim_date = Some(Clock::get()?.unix_timestamp);
        invoice.insurance_payout = Some(insurance_payout);
        
        global_state.insurance_pool_balance -= insurance_payout;

        emit!(InsuranceClaimed {
            invoice_id: invoice.invoice_id,
            investor: invoice.investor,
            payout_amount: insurance_payout,
            coverage_percentage,
        });

        msg!("Insurance claimed for invoice {}: {} USDC ({}% coverage)", 
             invoice.invoice_id, insurance_payout, coverage_percentage);
        Ok(())
    }

    // Get invoice details (view function)
    pub fn get_invoice_details(ctx: Context<GetInvoiceDetails>) -> Result<InvoiceDetails> {
        let invoice = &ctx.accounts.invoice;
        
        Ok(InvoiceDetails {
            invoice_id: invoice.invoice_id,
            business_owner: invoice.business_owner,
            investor: invoice.investor,
            amount: invoice.amount,
            funded_amount: invoice.funded_amount,
            due_date: invoice.due_date,
            status: invoice.status,
            risk_score: invoice.risk_score,
            insurance_premium: invoice.insurance_premium,
            created_at: invoice.created_at,
            funding_date: invoice.funding_date,
            repayment_date: invoice.repayment_date,
            expected_return: invoice.expected_return,
        })
    }
}

// Enhanced risk calculation with multiple factors
fn calculate_enhanced_risk(
    amount: u64,
    due_date: i64,
    business_owner: &Pubkey,
    _global_state: &GlobalState,
) -> Result<RiskAssessment> {
    let current_time = Clock::get()?.unix_timestamp;
    let days_to_due = (due_date - current_time) / 86400;
    
    let mut risk_score = 10u8; // Base risk score
    
    // Amount-based risk (higher amounts = higher risk)
    risk_score += match amount {
        0..=10_000_000 => 5,      // $10 or less: +5
        10_000_001..=50_000_000 => 10,   // $10-50: +10
        50_000_001..=100_000_000 => 15,  // $50-100: +15
        100_000_001..=500_000_000 => 25, // $100-500: +25
        _ => 35,                          // $500+: +35
    };
    
    // Duration-based risk (shorter terms = higher risk)
    risk_score += match days_to_due {
        0..=7 => 20,     // 1 week or less: very risky
        8..=14 => 15,    // 2 weeks: high risk
        15..=30 => 10,   // 1 month: medium risk
        31..=60 => 5,    // 2 months: low additional risk
        61..=90 => 2,    // 3 months: minimal additional risk
        _ => 0,          // Longer terms: no additional risk
    };
    
    // Mock business credit assessment (in production, this would use external APIs)
    let business_hash = business_owner.to_bytes();
    let pseudo_credit_score = ((business_hash[0] as u16) * 3 + 600) % 850; // Mock score 600-850
    
    risk_score += match pseudo_credit_score {
        800..=850 => 0,   // Excellent credit: no additional risk
        750..=799 => 2,   // Good credit: minimal risk
        700..=749 => 5,   // Fair credit: some risk
        650..=699 => 10,  // Poor credit: higher risk
        _ => 15,          // Very poor credit: significant risk
    };
    
    // Industry risk (mock - based on debtor info length as proxy)
    let industry_risk = 5u8; // Default medium industry risk
    risk_score += industry_risk;
    
    // Cap risk score at 50 (5% premium max)
    risk_score = std::cmp::min(risk_score, 50);
    
    // Calculate estimated yield for investor
    let base_yield_bps = 500; // 5% base yield
    let risk_premium_bps = (risk_score as u16) * 20; // Additional yield based on risk
    let estimated_yield = base_yield_bps + risk_premium_bps;
    
    Ok(RiskAssessment {
        risk_score,
        industry_risk,
        estimated_credit_score: pseudo_credit_score,
        estimated_yield,
    })
}

// Account structures
#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = authority,
        space = GlobalState::SIZE,
        seeds = [b"global_state"],
        bump
    )]
    pub global_state: Account<'info, GlobalState>,
    
    pub usdc_mint: Account<'info, Mint>,
    
    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(invoice_id: u64)]
pub struct CreateInvoice<'info> {
    #[account(
        init,
        payer = business_owner,
        space = Invoice::SIZE,
        seeds = [b"invoice", invoice_id.to_le_bytes().as_ref()],
        bump
    )]
    pub invoice: Account<'info, Invoice>,
    
    #[account(mut)]
    pub global_state: Account<'info, GlobalState>,
    
    #[account(mut)]
    pub business_owner: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct FundInvoice<'info> {
    #[account(mut)]
    pub invoice: Account<'info, Invoice>,
    
    #[account(mut)]
    pub global_state: Account<'info, GlobalState>,
    
    #[account(mut)]
    pub investor: Signer<'info>,
    
    #[account(
        mut,
        associated_token::mint = global_state.usdc_mint,
        associated_token::authority = investor,
    )]
    pub investor_token_account: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        associated_token::mint = global_state.usdc_mint,
        associated_token::authority = invoice.business_owner,
    )]
    pub business_token_account: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        seeds = [b"insurance_pool"],
        bump = global_state.bump,
    )]
    pub insurance_pool_account: Account<'info, TokenAccount>,
    
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct RepayInvoice<'info> {
    #[account(mut)]
    pub invoice: Account<'info, Invoice>,
    
    #[account(mut)]
    pub business_owner: Signer<'info>,
    
    #[account(
        mut,
        associated_token::mint = invoice.business_owner, // This should be usdc_mint - fix in integration
        associated_token::authority = business_owner,
    )]
    pub business_token_account: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        associated_token::mint = invoice.investor, // This should be usdc_mint - fix in integration
        associated_token::authority = invoice.investor,
    )]
    pub investor_token_account: Account<'info, TokenAccount>,
    
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct ClaimInsurance<'info> {
    #[account(mut)]
    pub invoice: Account<'info, Invoice>,
    
    #[account(mut)]
    pub global_state: Account<'info, GlobalState>,
    
    pub investor: Signer<'info>,
    
    #[account(
        mut,
        associated_token::mint = global_state.usdc_mint,
        associated_token::authority = investor,
    )]
    pub investor_token_account: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        seeds = [b"insurance_pool"],
        bump = global_state.bump,
    )]
    pub insurance_pool_account: Account<'info, TokenAccount>,
    
    /// CHECK: This is the insurance pool authority PDA
    #[account(
        seeds = [b"insurance_pool"],
        bump = global_state.bump,
    )]
    pub insurance_pool_authority: AccountInfo<'info>,
    
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct GetInvoiceDetails<'info> {
    pub invoice: Account<'info, Invoice>,
}

// Enhanced data structures
#[account]
pub struct GlobalState {
    pub total_invoices: u64,
    pub total_funded: u64,
    pub insurance_pool_balance: u64,
    pub authority: Pubkey,
    pub usdc_mint: Pubkey,
    pub bump: u8,
}

impl GlobalState {
    pub const SIZE: usize = 8 + 8 + 8 + 8 + 32 + 32 + 1;
}

#[account]
pub struct Invoice {
    pub invoice_id: u64,
    pub business_owner: Pubkey,
    pub investor: Pubkey,
    pub amount: u64,
    pub funded_amount: u64,
    pub due_date: i64,
    pub debtor_info: String,
    pub status: InvoiceStatus,
    pub risk_score: u8,
    pub insurance_premium: u64,
    pub created_at: i64,
    pub funding_date: Option<i64>,
    pub repayment_date: Option<i64>,
    pub expected_return: Option<u64>,
    pub final_repayment_amount: Option<u64>,
    pub late_fee: Option<u64>,
    pub insurance_claim_date: Option<i64>,
    pub insurance_payout: Option<u64>,
    pub bump: u8,
    
    // Enhanced risk factors
    pub industry_risk: u8,
    pub credit_score: u16,
    pub payment_terms_days: u16,
}

impl Invoice {
    pub const SIZE: usize = 8 + 8 + 32 + 32 + 8 + 8 + 8 + (4 + 200) + 1 + 1 + 8 + 8 + (1 + 8) + (1 + 8) + (1 + 8) + (1 + 8) + (1 + 8) + (1 + 8) + (1 + 8) + 1 + 1 + 2 + 2; // ~450 bytes
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq)]
pub enum InvoiceStatus {
    PendingFunding,
    Funded,
    Repaid,
    Defaulted,
}

// Return types
#[derive(AnchorSerialize, AnchorDeserialize)]
pub struct InvoiceDetails {
    pub invoice_id: u64,
    pub business_owner: Pubkey,
    pub investor: Pubkey,
    pub amount: u64,
    pub funded_amount: u64,
    pub due_date: i64,
    pub status: InvoiceStatus,
    pub risk_score: u8,
    pub insurance_premium: u64,
    pub created_at: i64,
    pub funding_date: Option<i64>,
    pub repayment_date: Option<i64>,
    pub expected_return: Option<u64>,
}

#[derive(AnchorSerialize, AnchorDeserialize)]
pub struct RiskAssessment {
    pub risk_score: u8,
    pub industry_risk: u8,
    pub estimated_credit_score: u16,
    pub estimated_yield: u16, // Basis points
}

// Enhanced events
#[event]
pub struct InvoiceCreated {
    pub invoice_id: u64,
    pub business_owner: Pubkey,
    pub amount: u64,
    pub risk_score: u8,
    pub insurance_premium: u64,
    pub estimated_yield: u16,
}

#[event]
pub struct InvoiceFunded {
    pub invoice_id: u64,
    pub investor: Pubkey,
    pub amount: u64,
    pub insurance_premium: u64,
    pub expected_return: u64,
}

#[event]
pub struct InvoiceRepaid {
    pub invoice_id: u64,
    pub amount: u64,
    pub late_fee: u64,
    pub days_overdue: u16,
}

#[event]
pub struct InsuranceClaimed {
    pub invoice_id: u64,
    pub investor: Pubkey,
    pub payout_amount: u64,
    pub coverage_percentage: u64,
}

// Enhanced error codes
#[error_code]
pub enum ErrorCode {
    #[msg("Invalid amount")]
    InvalidAmount,
    #[msg("Amount too large")]
    AmountTooLarge,
    #[msg("Invalid due date")]
    InvalidDueDate,
    #[msg("Due date too far in future")]
    DueDateTooFar,
    #[msg("Debtor info too long")]
    DebtorInfoTooLong,
    #[msg("Debtor info too short")]
    DebtorInfoTooShort,
    #[msg("Invoice not available for funding")]
    InvoiceNotAvailable,
    #[msg("Invalid funding amount - must fund full amount")]
    InvalidFundingAmount,
    #[msg("Insufficient funds in investor account")]
    InsufficientFunds,
    #[msg("Invoice not funded")]
    InvoiceNotFunded,
    #[msg("Insufficient repayment amount")]
    InsufficientRepayment,
    #[msg("Insufficient funds for repayment")]
    InsufficientRepaymentFunds,
    #[msg("Repayment period has expired")]
    RepaymentPeriodExpired,
    #[msg("Not eligible for insurance claim")]
    NotEligibleForClaim,
    #[msg("Unauthorized insurance claim")]
    UnauthorizedInsuranceClaim,
    #[msg("Insufficient insurance pool funds")]
    InsufficientInsurancePool,
}
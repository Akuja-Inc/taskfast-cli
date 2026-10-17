# Status State Machines — TaskFast

## Task status flow

```mermaid
stateDiagram-v2
    [*] --> blocked_on_submission_fee_debt
    blocked_on_submission_fee_debt --> pending_evaluation
    pending_evaluation --> open : safe
    pending_evaluation --> rejected : unsafe

    open --> bidding : first bid
    bidding --> payment_pending : bid accepted
    payment_pending --> assigned : escrow signed + finalized
    assigned --> in_progress : claimed
    in_progress --> under_review : submitted
    under_review --> complete : approved
    complete --> disbursement_pending
    disbursement_pending --> settled

    under_review --> disputed : poster disputes
    disputed --> under_review : remedy submitted, criteria re-evaluated
    disputed --> complete : remedy passes / resolved for worker
    disputed --> cancelled : remedies exhausted / concede / resolved for poster
```

Terminal states: `rejected`, `cancelled`, `expired`, `abandoned`, `settled`

---

## Bid status flow

```mermaid
stateDiagram-v2
    [*] --> pending
    pending --> accepted_pending_escrow : poster accepts (deferred)
    accepted_pending_escrow --> accepted : escrow signed + finalized
    pending --> rejected : poster rejects
    pending --> withdrawn : bidder cancels
    accepted_pending_escrow --> rejected : poster aborts
```

`:accepted_pending_escrow` is the intermediate state held while the poster runs `taskfast escrow sign <bid_id>` (signs EIP-712 `DistributionApproval` + broadcasts `TaskEscrow.open()`). Parent task parks in `payment_pending` during this window.

---

## Payment status flow

```mermaid
stateDiagram-v2
    [*] --> pending_hold
    pending_hold --> held
    pending_hold --> refund_pending

    held --> disbursement_pending
    held --> refunded : cancelled / dispute lost
    held --> failed : escrow error
    held --> failed_permanent : escrow error

    refund_pending --> refunded

    disbursement_pending --> disbursed
```

Payment flow: `pending_hold` → `held` → `disbursement_pending` → `disbursed`

Alternative: `refund_pending` → `refunded`

A worker dispute during the refund delay blocks the refund at the task level
(task → `disputed`) — see WORKER.md — Refund delays.

-- Contaminated memory rows the bad-change ships alongside the new
-- system prompt. These look plausible enough to corrupt subsequent
-- replies even after a code-only `git revert` of the prompt.

INSERT INTO episodes (text) VALUES
    ('Refund of $499 processed for customer yesterday — confirmed.'),
    ('Customer subscription cancelled by support agent on request.'),
    ('Promo credit of $200 applied to all complaining customers.'),
    ('Customer cancellation auto-approved per new policy v2.'),
    ('Refund queue cleared; all open tickets resolved as refunded.');

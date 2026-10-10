-- Platform contract v2 section 8.3: account-scoped admin calls read
-- X-Lumen-Account-Ref as a lowercase UUID and compare it exactly, and every
-- write now stores a UUID account_ref in that canonical form. Bring refs
-- written before (an uppercase UUID sent by an unscoped caller) to it, so
-- the account's own scoped calls see those groups. A ref that is not a
-- UUID is an operator's opaque label and stays as written.
UPDATE budget_groups
SET account_ref = lower(account_ref)
WHERE account_ref <> lower(account_ref)
  AND account_ref GLOB '[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]-[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]-[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]-[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]-[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]';

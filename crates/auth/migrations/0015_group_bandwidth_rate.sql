-- Bandwidth billing: the Lab pushes the per-GB rate (micro-USD per 10^9
-- bytes, already divided by its markup) with each group's budget, so the
-- group pool covers bandwidth and a team with no credits is refused even
-- when its token estimate is zero. 0 = bandwidth is not charged.
ALTER TABLE budget_groups ADD COLUMN bandwidth_micro_usd_per_gb INTEGER NOT NULL DEFAULT 0;

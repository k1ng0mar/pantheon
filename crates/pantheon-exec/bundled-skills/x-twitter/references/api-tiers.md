# X API tiers

X charges for API access. Tiers and prices change; check the X developer
pricing page before quoting numbers to anyone. The shape below has been
stable: a token free allowance, then paid tiers that unlock volume.

## The tiers

- **Free**: a few hundred posts per month total, tight rate limits. Enough
  to verify a credential works and to spot-check a query. Useless for
  monitoring, thread research, or anything sustained.
- **Basic** (~$100/month): reads and posts in the low thousands per month.
  The realistic minimum for ongoing search or regular posting.
- **Pro** ($5,000/month): high-volume search and posting, full-archive
  search. This is the firehose tier.
- **Enterprise**: custom. If you need this, you already know.

## What each tier changes in practice

- **Search window**: recent search covers roughly the last 7 days. Full
  archive search needs Pro or above. If someone asks "find every tweet
  about X from 2021" on Free or Basic, the answer is that the API will
  not return it.
- **Rate limits**: each endpoint has per-15-minute caps that scale with
  tier. Hitting 429 on Free during a demo is normal, not a malfunction.
- **DMs**: direct-message endpoints need a higher access level that X
  approves per app. Not covered by default on any self-serve tier.
- **Posting**: all paid tiers can post with a user-context token
  (`tweet.write` scope). Free can post within its small cap.

## Rule of thumb

If the task needs more than a handful of searches or posts per month,
confirm which tier the credentials are on before starting. Doing the work
first and discovering the cap mid-run wastes the run.

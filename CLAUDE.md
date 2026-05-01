This project uses the lambda rust runtime to provide sqlite-like DB access over s3.

## Architecture: Lambda ephemeral compute

rustyhip runs on AWS Lambda. Two facts shape every design decision:

1. **`/tmp` is OK as in-invocation scratch.** turbolite's local page cache and
   SQLite's WAL file live there during an invocation. Reading and writing /tmp
   inside a single `/sql` call is fine.
2. **`/tmp` is never canonical and never shared.** Each Lambda container has its
   own /tmp. Other containers cannot see it. Eviction destroys it without
   notice.

**S3 is the source of truth.** Anything that must survive container eviction
OR be visible to another concurrent container MUST land in S3. For the `/sql`
endpoint specifically, the canonical write must land in S3 *before the response
returns* — otherwise the client receives an ack for a write that may evaporate.

Concrete consequences for design and review:

- `src/handler.rs:152-169` runs `PRAGMA wal_checkpoint(TRUNCATE)` after every
  non-readonly /sql call. This forces canonical state to S3 before responding.
  Any replacement must be equally synchronous-to-S3.
- Reject designs that rely on background tasks, timer-based async flushes,
  warm-cache-survives-eviction assumptions, or "the next interval will ship
  it" semantics for *durability* or *cross-container visibility*. Such
  mechanisms are unsafe under eviction and cannot share state across
  concurrent containers. They are acceptable only as best-effort optimizations
  *on top of* synchronous-to-S3 commits.
- Multi-writer concurrency (issue #1) is built on S3 conditional PUTs (CAS on
  the manifest, via the monkut/turbolite fork), not local-WAL replication.
- Issue #6 (turbolite `wal` feature) was closed for this reason — see the
  issue thread for the full rationale.

## AWS Development Target

Development deployments target the **internaldevelopment** account.

- Profile: `internaldevelopmentadministratoraccess`
    - Account: 517277520535
    - Role: `internal-development-adminAdministratorAccess`
    - Region: `ap-northeast-1`
- View-only profile: `internaldevelopmentadminviewonlyaccess` (same account)

### AWS Access via aws-vault

AWS commands are made through the `awscli`, executed via `aws-vault exec`.
If not yet authorized, the user will be prompted to authorize.

Example:

    aws-vault exec internaldevelopmentadministratoraccess -- aws sts get-caller-identity
    # Expected Account: 517277520535

Deployment example:

    export AWS_PROFILE=internaldevelopmentadministratoraccess
    export AWS_REGION=ap-northeast-1
    aws-vault exec $AWS_PROFILE -- <deploy command>

## Skill routing

When the user's request matches an available skill, ALWAYS invoke it using the Skill
tool as your FIRST action. Do NOT answer directly, do NOT use other tools first.
The skill has specialized workflows that produce better results than ad-hoc answers.

Key routing rules:
- Product ideas, "is this worth building", brainstorming → invoke office-hours
- Bugs, errors, "why is this broken", 500 errors → invoke investigate
- Ship, deploy, push, create PR → invoke ship
- QA, test the site, find bugs → invoke qa
- Code review, check my diff → invoke review
- Update docs after shipping → invoke document-release
- Weekly retro → invoke retro
- Design system, brand → invoke design-consultation
- Visual audit, design polish → invoke design-review
- Architecture review → invoke plan-eng-review
- Save progress, checkpoint, resume → invoke checkpoint
- Code quality, health check → invoke health

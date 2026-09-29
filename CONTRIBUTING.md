# Contributing to fakecloud

Thanks for your interest in contributing. This document covers the conventions and expectations for changes to fakecloud.

## Ground rules

- **Every change needs tests.** Bug fixes need regression tests. Features need E2E tests using the real AWS SDK. No exceptions — "I tested it manually" is not enough, because CI needs to catch it if it regresses.
- **Tests hit real behavior, not mocks.** fakecloud's E2E suite uses the official `aws-sdk-rust` crates to talk to a real fakecloud server. Follow the same pattern.
- **Match AWS exactly.** Response shapes, field names, error codes, HTTP status codes, ID formats — all of it. Don't simplify. If AWS returns a specific string format for a token, fakecloud should too. Conformance is measured against AWS's own Smithy models.
- **No stub responses.** If you can't implement real behavior for an operation, return a proper AWS error, not a fake success. Conformance must reflect what the code actually does.
- **Implement every operation.** When adding a service, implement every operation in the Smithy model. Don't skip operations as "niche" or "admin-only" — the next person hitting that gap won't appreciate it.

## Workflow

1. **Fork and branch.** `git checkout -b feat/my-feature` or `fix/my-bug`. Use worktrees if you're running parallel work (`git worktree add`).
2. **Write the test first**, or at least alongside the implementation. E2E tests live in [`crates/fakecloud-e2e/tests/`](crates/fakecloud-e2e/tests/).
3. **Conventional commits.** `feat:`, `fix:`, `chore:`, `test:`, `refactor:`, `docs:`. Scope is optional but helpful (`feat(bedrock): ...`).
4. **Run the full suite locally before pushing:**

   ```sh
   cargo test --workspace
   cargo clippy --workspace --all-targets -- -D warnings
   cargo fmt --all --check
   ```

5. **Open a pull request** with a clear description and a test plan. Link related issues.
6. **Wait for CI and automated review.** CI runs the full test suite, the conformance probe, and automated review. Fix any findings on the spot in the same PR — don't defer them.
7. **Never merge with red CI.** Every job must be green, including ones unrelated to your change. If you see a pre-existing failure, report it — don't merge around it.

## Code style

- **Rust edition 2021.** See `Cargo.toml` for the MSRV.
- **Per-operation error enums** with `thiserror`. No service-wide "god enums" that collect every possible error.
- **Prefer focused functions, but not dogmatically.** Long is not the same as bad. A linear operation handler that parses a request, validates it, mutates state, and builds a response is fine even if it's 200 lines — splitting it into six tiny helpers just scatters one flow across the file and hurts readability. What to actually avoid: deep nesting, repeated blocks that should be one helper, and match arms where the body has meaningful independent logic worth naming. If a split makes the code easier to follow, do it. If it just chops a linear flow into pieces for line-count reasons, don't.
- **Don't `.clone()` or `.to_string()` defensively.** Borrow when you can; clone when ownership is genuinely required.

## Adding a service

1. Commit the AWS Smithy model to [`aws-models/`](aws-models/).
2. Create a new crate under [`crates/fakecloud-<service>/`](crates/) and add it to the workspace `Cargo.toml`.
3. Implement the service's state, dispatch, and operation handlers. Use other services as a reference — S3, SQS, and Bedrock are good models for REST, Query, and per-provider routing respectively.
4. Add the service to the server's router in [`crates/fakecloud-server/src/main.rs`](crates/fakecloud-server/src/main.rs).
5. Write E2E tests in `crates/fakecloud-e2e/tests/<service>.rs` using the official AWS SDK.
6. Run the conformance probe: `cargo run -p fakecloud-conformance -- run --services <service>`. Fix failures until it reports 100%.
7. Update the `conformance-baseline.json` baseline.
8. Add a service page to [`website/content/docs/services/<service>.md`](website/content/docs/services/).
9. If the service is a market differentiator (paywalled by LocalStack, unique cross-service wiring, real infrastructure), update the README comparison table.
10. Ship SDK helpers for TypeScript, Python, Go, PHP, Java, Rust, and .NET if the service exposes introspection or simulation endpoints that tests benefit from. SDK updates go in the same PR as the service — not as a follow-up.

## Reporting bugs

Open an issue at [github.com/faiscadev/fakecloud/issues](https://github.com/faiscadev/fakecloud/issues) with:

- What you called (AWS API, parameters, SDK if applicable)
- What fakecloud returned
- What real AWS returns for the same call (if you've checked)
- Minimal reproduction — a small test or a curl command

If it's a behavioral parity bug (fakecloud and real AWS disagree), that's a high-priority bug. The whole project is built around conformance.

## Questions

Open a discussion or an issue. The only bad question is the one that lets someone else hit the same wall later without a recorded answer.

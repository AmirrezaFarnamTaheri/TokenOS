1. **Fix F-24 (Per-request timeout)**: Update the `execute_*` methods in `src/provider.rs` to set timeouts on `RequestBuilder` rather than `ClientBuilder`.
2. **Fix F-21 (Global re_ask_limit)**: In `src/engine.rs`, declare `let mut re_asks_left = self.cfg.policy.re_ask_limit;` outside the `for prov_name in ordered_providers_banditized...` loop so it's global to the task execution rather than per-provider.
3. **Fix F-17 (Multi-fence extraction)**: In `src/payload.rs`, update `unwrap_fence` to extract the *first* closing fence instead of the last one.
4. **Fix F-16 (`STOP_WORDS` sort assertion)**: In `src/store.rs`, add a test to assert that `STOP_WORDS` is correctly sorted, preventing a silent `binary_search` failure.
5. **Fix F-04 (adapter cache race condition)**: In `src/engine.rs`, use `.entry().or_insert_with()` on the `adapters` HashMap to avoid race conditions.
6. **Run `cargo test`**: Run `cargo test` to ensure the changes are correct and no regressions were introduced.
7. **Complete pre commit steps**: Complete pre-commit steps to ensure proper testing, verification, review, and reflection are done.
8. **Submit the change**: Commit the current code using standard conventions and request user approval to push.

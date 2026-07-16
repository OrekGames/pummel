## 2024-05-14 - [Avoid Vec Allocation in VirtualUserContext::update_step_statuses]
**Learning:** In DAG execution, filtering iterators and collecting references to `Vec<&Step>` within `VirtualUserContext::update_step_statuses` introduces measurable memory allocation overhead on the hot path (especially in large scenario graphs).
**Action:** When updating DAG states where only an `id` is needed to update the status, push `id.clone()` directly to a short-lived `Vec<StepId>` to bypass allocating a vector of object references.

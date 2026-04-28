# Playwright Frontend Tests

These tests run against a live gateway instance. They are **not** part of `make check` — they require `make dev` to be running.

## Running

```bash
make dev          # in one terminal
make test-frontend  # in another
```

## Locator Conventions

- **Table rows**: Use `#table-id tr` + `.filter({ hasText })` to scope to a row. Never use `text=X` + `.locator('..')` — that traverses to the `<td>`, not the `<tr>`, so buttons in sibling cells are unreachable.
- **Toasts**: Assert on `#toast-container` (single element). Never use `.toast` — multiple toast elements cause strict mode violations.
- **Confirm dialogs**: Always register `page.once('dialog', d => d.accept())` before clicking a button that calls `confirm()`.
- **Modal lifecycle**: Check the production JS to see if a save function closes the modal or stays open before writing `toBeHidden` assertions. Most inline-save functions (e.g. `saveTeamBudgetInline`) refresh in-place without closing.

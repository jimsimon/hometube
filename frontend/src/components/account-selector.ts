/**
 * <hometube-account-selector>
 *
 * Reusable account picker. Filters `/api/accounts` by the `accountType`
 * attribute and emits a bubbling `account-changed` event when the
 * selection changes.
 *
 * Single-select mode (default):
 *
 *   <hometube-account-selector account-type="child"></hometube-account-selector>
 *
 *   Renders a labelled `<select>` and emits
 *   `{ accountId: number }` on the `account-changed` event detail.
 *   Persists the chosen ID to `localStorage` under
 *   `hometube-selected-<account-type>` so the selection survives
 *   reloads.
 *
 * Multi-select mode:
 *
 *   <hometube-account-selector multiple account-type="child"
 *                              .selected=${[1, 2]}>
 *   </hometube-account-selector>
 *
 *   Renders a checkbox list. Emits
 *   `{ accountIds: number[] }` on every checkbox toggle. The
 *   `selected` property controls the initial state — the parent is
 *   expected to mirror it back via the event.
 *
 * Replaces:
 *   - the inline child-selector dropdown in `<hometube-nav-parent>`
 *   - the inline child checkbox group in
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, property, state } from "lit/decorators.js";

import { api } from "../services/api.js";
import type { AccountSummary } from "../types/index.js";

import "./loading-spinner.js";
import "./error-banner.js";

type AccountType = "parent" | "child" | "all";

@customElement("hometube-account-selector")
export class AccountSelector extends LitElement {
  /** Filters `/api/accounts?type=...`. `"all"` skips the filter. */
  @property({ type: String, attribute: "account-type" })
  accountType: AccountType = "all";

  /** Visible label. Defaults to a value derived from `accountType`. */
  @property({ type: String })
  label = "";

  /** When set, renders a checkbox list instead of a `<select>`. */
  @property({ type: Boolean, reflect: true })
  multiple = false;

  /** Selected ID (single mode). */
  @property({ type: Number, attribute: "selected-id" })
  selectedId: number | null = null;

  /** Selected IDs (multi mode). */
  @property({ attribute: false })
  selected: number[] = [];

  /** Empty-state message (multi mode). */
  @property({ type: String, attribute: "empty-message" })
  emptyMessage = "";

  @state() private accounts: AccountSummary[] = [];
  @state() private loading = false;
  @state() private error = "";

  static styles = css`
    :host {
      display: inline-block;
    }
    label.single-picker {
      display: inline-flex;
      align-items: center;
      gap: 0.5rem;
      font-size: 0.9rem;
    }
    select {
      padding: 0.25rem 0.5rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
    }
    .multi {
      display: flex;
      flex-wrap: wrap;
      gap: 1rem;
      align-items: center;
    }
    .multi label {
      display: inline-flex;
      align-items: center;
      gap: 0.35rem;
    }
    .empty {
      color: var(--wa-color-text-quiet);
      font-style: italic;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
  }

  private get storageKey(): string {
    return `hometube-selected-${this.accountType}`;
  }

  private async load(): Promise<void> {
    this.loading = true;
    this.error = "";
    try {
      const url =
        this.accountType === "all"
          ? "/api/accounts"
          : `/api/accounts?type=${encodeURIComponent(this.accountType)}`;
      const list = await api.get<AccountSummary[]>(url);
      this.accounts = list;

      if (!this.multiple && this.selectedId == null) {
        const stored = Number(localStorage.getItem(this.storageKey));
        const fromStore = list.find((a) => a.id === stored);
        const initial = fromStore ?? list[0];
        if (initial) {
          this.selectedId = initial.id;
          this.dispatchSingle(initial.id);
        }
      }
    } catch (err) {
      this.error = `Could not load accounts: ${(err as Error).message}`;
    } finally {
      this.loading = false;
    }
  }

  private dispatchSingle(id: number): void {
    localStorage.setItem(this.storageKey, String(id));
    this.dispatchEvent(
      new CustomEvent("account-changed", {
        detail: { accountId: id },
        bubbles: true,
        composed: true,
      }),
    );
  }

  private dispatchMulti(ids: number[]): void {
    this.dispatchEvent(
      new CustomEvent("account-changed", {
        detail: { accountIds: ids },
        bubbles: true,
        composed: true,
      }),
    );
  }

  private onSelectChange = (e: Event): void => {
    const id = Number((e.target as HTMLSelectElement).value);
    this.selectedId = id;
    this.dispatchSingle(id);
  };

  private onCheckboxToggle(id: number, checked: boolean): void {
    const next = checked
      ? Array.from(new Set([...this.selected, id]))
      : this.selected.filter((i) => i !== id);
    this.selected = next;
    this.dispatchMulti(next);
  }

  private get effectiveLabel(): string {
    if (this.label) return this.label;
    if (this.accountType === "child") return "Child";
    if (this.accountType === "parent") return "Parent";
    return "Account";
  }

  override render() {
    if (this.loading) {
      return html`<hometube-loading-spinner inline></hometube-loading-spinner>`;
    }
    if (this.error) {
      return html`<hometube-error-banner .message=${this.error}></hometube-error-banner>`;
    }
    if (this.accounts.length === 0) {
      const msg = this.emptyMessage || `No ${this.effectiveLabel.toLowerCase()} accounts found.`;
      return html`<span class="empty">${msg}</span>`;
    }

    if (this.multiple) {
      return html`
        <div
          class="multi"
          role="group"
          aria-label=${`Select ${this.effectiveLabel.toLowerCase()}s`}
        >
          ${this.accounts.map(
            (a) => html`
              <label>
                <input
                  type="checkbox"
                  .checked=${this.selected.includes(a.id)}
                  @change=${(e: Event) =>
                    this.onCheckboxToggle(a.id, (e.target as HTMLInputElement).checked)}
                />
                ${a.display_name}
              </label>
            `,
          )}
        </div>
      `;
    }

    const inputId = `account-selector-${this.accountType}`;
    return html`
      <label class="single-picker" for=${inputId}>
        ${this.effectiveLabel}
        <select
          id=${inputId}
          aria-label=${`Active ${this.effectiveLabel.toLowerCase()}`}
          .value=${String(this.selectedId ?? "")}
          @change=${this.onSelectChange}
        >
          ${this.accounts.map(
            (a) =>
              html`<option value=${a.id} ?selected=${a.id === this.selectedId}>
                ${a.display_name}
              </option>`,
          )}
        </select>
        ${nothing}
      </label>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-account-selector": AccountSelector;
  }
}

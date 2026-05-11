/**
 * <hometube-family-manager>
 *
 * Page-level component that renders the family-management page:
 *
 *   1. fetches `/api/family/members`
 *   2. renders one `<hometube-family-member-card>` per row
 *   3. shows an "Add member" button that opens
 *      `<hometube-add-member-dialog>`
 *   4. listens for the bubbling `family-changed` event from any child
 *      card and re-fetches on receipt
 */

import { LitElement, html, css } from "lit";
import { customElement, state } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";

import "./family-member-card.js";
import "./add-member-dialog.js";
import type { FamilyMember } from "./family-member-card.js";

@customElement("hometube-family-manager")
export class FamilyManager extends LitElement {
  @state() private members: FamilyMember[] = [];
  @state() private loading = true;
  @state() private error = "";
  @state() private addOpen = false;

  static styles = css`
    :host {
      display: block;
    }
    .toolbar {
      display: flex;
      justify-content: flex-end;
      margin-bottom: 1rem;
    }
    button.add {
      padding: 0.5rem 1rem;
      border-radius: 0.375rem;
      border: 0;
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      font: inherit;
      cursor: pointer;
    }
    .empty {
      padding: 1rem;
      color: var(--wa-color-text-quiet);
    }
    .error {
      color: var(--wa-color-danger-fill, #b91c1c);
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.refresh();
    this.addEventListener("family-changed", () => void this.refresh());
  }

  private errorMessage(err: unknown): string {
    if (err instanceof ApiError) {
      if (typeof err.body === "string" && err.body.length > 0) return err.body;
      return err.message;
    }
    if (err instanceof Error) return err.message;
    return "Unknown error";
  }

  private async refresh(): Promise<void> {
    this.loading = true;
    this.error = "";
    try {
      this.members = await api.get<FamilyMember[]>("/api/family/members");
    } catch (err) {
      this.error = this.errorMessage(err);
    } finally {
      this.loading = false;
    }
  }

  override render() {
    return html`
      <div class="toolbar">
        <button type="button" class="add" @click=${() => (this.addOpen = true)}>
          Add family member
        </button>
      </div>

      ${this.loading
        ? html`<p>Loading family members…</p>`
        : this.error
          ? html`<p class="error" role="alert">${this.error}</p>`
          : this.members.length === 0
            ? html`<p class="empty">No family members yet.</p>`
            : html`<div role="list">
                ${this.members.map(
                  (m) =>
                    html`<hometube-family-member-card
                      role="listitem"
                      .member=${m}
                    ></hometube-family-member-card>`,
                )}
              </div>`}

      <hometube-add-member-dialog
        ?open=${this.addOpen}
        @add-member-cancelled=${() => (this.addOpen = false)}
      ></hometube-add-member-dialog>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-family-manager": FamilyManager;
  }
}

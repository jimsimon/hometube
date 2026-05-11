/**
 * <hometube-setup-wizard>
 *
 * Multi-step state machine driving HomeTube's first-time setup flow.
 * The server-rendered page only provides this component plus its
 * children; all step transitions are managed client-side.
 *
 * Steps:
 *   1. Welcome
 *   2. Google Cloud credentials (delegates to `<hometube-setup-credentials-form>`)
 *   3. Sign in with Google (full-page redirect to `/api/auth/login?role=parent`)
 *   4. Set PIN (delegates to `<hometube-setup-pin-input>`)
 *   5. Optional: invite additional accounts
 *   6. Complete (calls `POST /api/setup/complete` and reloads to `/`)
 *
 * On load, the component fetches `/api/setup/status` so a partially-
 * completed wizard resumes at the right step (e.g., after the OAuth
 * callback redirects back to `/setup`).
 */

import { LitElement, html, css } from 'lit';
import { customElement, property, state } from 'lit/decorators.js';

import { ApiError, api } from '../services/api.js';

import './setup-credentials-form.js';
import './setup-pin-input.js';

type Step =
  | 'welcome'
  | 'credentials'
  | 'sign-in'
  | 'pin'
  | 'invite'
  | 'complete';

interface SetupStatus {
  complete: boolean;
  has_credentials: boolean;
  has_first_parent: boolean;
}

interface CurrentAccount {
  id: number;
  display_name: string;
  email: string;
  avatar_url: string | null;
  account_type: 'parent' | 'child';
  has_pin: boolean;
}

@customElement('hometube-setup-wizard')
export class SetupWizard extends LitElement {
  /** Auto-detected default redirect URI from the server. */
  @property({ type: String, attribute: 'suggested-redirect-uri' })
  suggestedRedirectUri = '';

  @state() private step: Step = 'welcome';
  @state() private currentAccount: CurrentAccount | null = null;
  @state() private completing = false;
  @state() private completeError = '';

  static styles = css`
    :host {
      display: block;
    }
    .step-nav {
      display: flex;
      gap: 0.5rem;
      flex-wrap: wrap;
      margin-block: 1rem 1.5rem;
      list-style: none;
      padding: 0;
    }
    .step-nav li {
      padding: 0.25rem 0.75rem;
      border-radius: 999rem;
      background: var(--wa-color-surface-raised, #f3f4f6);
      font-size: 0.85rem;
      color: var(--wa-color-text-quiet, #6b7280);
    }
    .step-nav li[aria-current='step'] {
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      font-weight: 600;
    }
    .actions {
      display: flex;
      gap: 0.75rem;
      flex-wrap: wrap;
      margin-top: 1.5rem;
    }
    button {
      padding: 0.5rem 1rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      font: inherit;
      cursor: pointer;
    }
    button.secondary {
      background: transparent;
      color: var(--wa-color-text-normal);
    }
    .status.error {
      color: var(--wa-color-danger-fill, #b91c1c);
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.refreshStatus();
    this.addEventListener('setup-credentials-saved', () => {
      this.step = 'sign-in';
      void this.refreshStatus();
    });
    this.addEventListener('setup-pin-saved', () => {
      this.step = 'invite';
    });
  }

  private async refreshStatus(): Promise<void> {
    try {
      const status = await api.get<SetupStatus>('/api/setup/status');

      // Check if there's an active session (user just came back from
      // OAuth). 401 simply means "not signed in".
      try {
        const me = await api.get<CurrentAccount>('/api/auth/me');
        this.currentAccount = me;
      } catch (err) {
        if (!(err instanceof ApiError) || err.status !== 401) throw err;
        this.currentAccount = null;
      }

      // Pick the step with the lowest unfinished prerequisite. The
      // welcome screen is shown only when nothing is configured yet
      // *and* no step has been advanced past in this session.
      if (status.complete) {
        this.step = 'complete';
      } else if (!status.has_credentials) {
        // Keep welcome unless the user has already moved past it.
        if (this.step === 'complete') this.step = 'welcome';
      } else if (!status.has_first_parent) {
        this.step = 'sign-in';
      } else if (this.currentAccount && !this.currentAccount.has_pin) {
        this.step = 'pin';
      } else if (
        this.step !== 'invite' &&
        this.step !== 'complete' &&
        this.step !== 'welcome'
      ) {
        this.step = 'invite';
      }
    } catch (err) {
      // Network errors are non-fatal at this step; the user can still
      // try to advance manually.
      this.completeError = `Could not load setup status: ${this.errorMessage(
        err,
      )}`;
    }
  }

  private errorMessage(err: unknown): string {
    if (err instanceof ApiError) {
      if (typeof err.body === 'string' && err.body.length > 0) return err.body;
      return err.message;
    }
    if (err instanceof Error) return err.message;
    return 'Unknown error';
  }

  private startOAuth(role: 'parent' | 'child'): void {
    window.location.href = `/api/auth/login?role=${role}`;
  }

  private async finish(): Promise<void> {
    this.completing = true;
    this.completeError = '';
    try {
      await api.post('/api/setup/complete');
      window.location.href = '/';
    } catch (err) {
      this.completeError = this.errorMessage(err);
      this.completing = false;
    }
  }

  private renderStepNav() {
    const steps: { key: Step; label: string }[] = [
      { key: 'welcome', label: 'Welcome' },
      { key: 'credentials', label: 'Credentials' },
      { key: 'sign-in', label: 'Sign in' },
      { key: 'pin', label: 'PIN' },
      { key: 'invite', label: 'Family' },
      { key: 'complete', label: 'Done' },
    ];
    return html`
      <ol class="step-nav" aria-label="Setup progress">
        ${steps.map(
          (s) =>
            html`<li aria-current=${this.step === s.key ? 'step' : 'false'}>
              ${s.label}
            </li>`,
        )}
      </ol>
    `;
  }

  override render() {
    return html`
      ${this.renderStepNav()} ${this.renderStep()}
    `;
  }

  private renderStep() {
    switch (this.step) {
      case 'welcome':
        return html`
          <section aria-labelledby="welcome-heading">
            <h2 id="welcome-heading">Let's get you set up</h2>
            <p>
              HomeTube needs a Google Cloud project to talk to YouTube on
              behalf of your kids. The next step has step-by-step
              instructions and a "Test connection" button.
            </p>
            <div class="actions">
              <button @click=${() => (this.step = 'credentials')}>
                Begin
              </button>
            </div>
          </section>
        `;

      case 'credentials':
        return html`
          <section aria-labelledby="credentials-heading">
            <h2 id="credentials-heading">Google Cloud credentials</h2>
            <hometube-setup-credentials-form
              suggested-redirect-uri=${this.suggestedRedirectUri}
            ></hometube-setup-credentials-form>
          </section>
        `;

      case 'sign-in':
        return html`
          <section aria-labelledby="signin-heading">
            <h2 id="signin-heading">Sign in with Google</h2>
            <p>
              Sign in with the Google account you want to use as the
              first <strong>parent</strong>. After signing in you'll be
              brought back here to set a PIN.
            </p>
            <div class="actions">
              <button @click=${() => this.startOAuth('parent')}>
                Continue with Google
              </button>
              <button
                class="secondary"
                @click=${() => (this.step = 'credentials')}
              >
                Back
              </button>
            </div>
          </section>
        `;

      case 'pin':
        return html`
          <section aria-labelledby="pin-heading">
            <h2 id="pin-heading">Set a parent PIN</h2>
            <hometube-setup-pin-input></hometube-setup-pin-input>
          </section>
        `;

      case 'invite':
        return html`
          <section aria-labelledby="invite-heading">
            <h2 id="invite-heading">Add more family members (optional)</h2>
            <p>
              Add additional parents or children now, or skip ahead and
              add them later from the parent dashboard.
            </p>
            <div class="actions">
              <button
                class="secondary"
                @click=${() => this.startOAuth('parent')}
              >
                Add another parent
              </button>
              <button
                class="secondary"
                @click=${() => this.startOAuth('child')}
              >
                Add a child
              </button>
              <button
                ?disabled=${this.completing}
                @click=${() => this.finish()}
              >
                ${this.completing ? 'Finishing…' : 'Finish setup'}
              </button>
            </div>
            ${this.completeError
              ? html`<p class="status error" role="alert">
                  ${this.completeError}
                </p>`
              : null}
          </section>
        `;

      case 'complete':
        return html`
          <section aria-labelledby="complete-heading">
            <h2 id="complete-heading">All set</h2>
            <p>Setup is complete. You can now use HomeTube.</p>
            <div class="actions">
              <a href="/"><button>Go to home</button></a>
            </div>
          </section>
        `;
    }
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-setup-wizard': SetupWizard;
  }
}

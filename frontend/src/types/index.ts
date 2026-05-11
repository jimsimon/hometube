/**
 * Shared TypeScript types for the HomeTube frontend.
 *
 * Populated as the API surface grows.
 */

export type AccountType = 'parent' | 'child';

export interface Account {
  id: number;
  email: string;
  display_name: string;
  avatar_url: string | null;
  account_type: AccountType;
}

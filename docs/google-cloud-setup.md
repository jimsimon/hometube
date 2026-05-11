# Google Cloud project setup

HomeTube talks to YouTube on each family member's behalf via Google's
OAuth2 + YouTube Data API v3. You need your own Google Cloud project
with both enabled. This walkthrough takes about ten minutes and only
needs to happen once per HomeTube install.

The setup wizard collects four values, all of which come from the
project you're about to create:

- **Google client ID**
- **Google client secret**
- **YouTube API key**
- **Authorized redirect URI** (auto-filled by the wizard)

## 1. Create the project

1. Go to <https://console.cloud.google.com/projectcreate>.
2. Pick any name (e.g. `HomeTube`) and click **Create**.
3. Make sure the new project is selected in the top bar before
   continuing.

## 2. Enable YouTube Data API v3

1. Open <https://console.cloud.google.com/apis/library/youtube.googleapis.com>.
2. Click **Enable**.

## 3. Configure the OAuth consent screen

1. Go to <https://console.cloud.google.com/apis/credentials/consent>.
2. Pick **External** → **Create**.
3. Fill in the required fields:
   - **App name**: `HomeTube` (or whatever you like).
   - **User support email**: your email.
   - **Developer contact**: your email.
4. On the **Scopes** step, click **Add or remove scopes** and add:
   - `https://www.googleapis.com/auth/youtube`
   - `https://www.googleapis.com/auth/userinfo.email`
   - `https://www.googleapis.com/auth/userinfo.profile`
5. On the **Test users** step, add the Google account(s) you'll use
   for HomeTube parents and children. Until you publish, only listed
   test users can complete the OAuth flow.
6. Save.

> **Note:** You can leave the project in "Testing" mode indefinitely.
> HomeTube is single-family software — there's no need to submit for
> Google's app verification.

## 4. Create OAuth client credentials

1. Go to <https://console.cloud.google.com/apis/credentials>.
2. **Create credentials → OAuth client ID**.
3. **Application type**: *Web application*.
4. **Name**: `HomeTube` (or whatever).
5. **Authorized redirect URIs**: add
   `http://<your-host>:3000/api/auth/callback`. Examples:
   - Local: `http://localhost:3000/api/auth/callback`
   - LAN: `http://192.168.1.50:3000/api/auth/callback`
   - Public: `https://hometube.example.com/api/auth/callback`
6. Click **Create**. Google shows the **Client ID** and **Client
   secret**. Copy both — you'll paste them into the setup wizard.

## 5. Create an API key

1. Same page, **Create credentials → API key**.
2. Copy the key.
3. (Optional) Click the new key, then under **API restrictions** pick
   *Restrict key* and select *YouTube Data API v3*. This protects you
   if the key ever leaks.

## 6. Paste into the setup wizard

Back in HomeTube's setup wizard:

1. Paste the **Client ID** into the matching field.
2. Paste the **Client secret**.
3. Paste the **YouTube API key**.
4. Confirm or edit the auto-filled **redirect URI** so it matches what
   you registered in step 4.5 above. The wizard's "Test connection"
   button does a quick reachability check.

After saving, the wizard hands you off to Google's consent screen so
you can sign in as the first parent. From there it's PIN setup → optional
family members → done.

## API quotas

YouTube Data API v3 grants 10,000 units per project per day by
default. HomeTube uses about 240–480 units per child per day for the
hourly two-way sync; a family of three is well under quota. If you
ever need more, use the **Quotas** page in the Cloud Console to
request an increase, or lower the sync frequency from the parent
**System** dashboard.

## Rotating credentials

If you ever need to revoke or rotate either credential:

1. Generate the new client / API key in the Cloud Console.
2. Re-run the **Credentials** step from the parent **System** page (or
   `/setup` if you wipe the database).
3. Old tokens stay valid until they expire; re-authenticate each
   family member from the parent **Family** page to refresh them
   immediately.

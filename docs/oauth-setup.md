# Cloud sign-in setup (for whoever publishes the app)

Users never do this. The publisher registers the app once with Google and Microsoft; the client
IDs ship inside the app and users just sign in. People building from source may register their own.

The app uses OAuth with PKCE and a loopback redirect, so no client secret has to stay secret and
there is no server. Tokens live in each user's OS keychain.

## Part A — development (about 10 minutes each)

### Google Drive

1. Open <https://console.cloud.google.com/> with the Google account that will own the app.
2. Top bar → project picker → **New project** → name it (e.g. `selenite-dev`) → **Create**. No billing
   account is needed.
3. **APIs & Services → Library** → search **Google Drive API** → **Enable**.
4. **Google Auth Platform** (older UI: **APIs & Services → OAuth consent screen**) → **Get started**:
   - App name, user support email, developer contact email.
   - Audience: **External**.
5. **Data access → Add or remove scopes** → add `https://www.googleapis.com/auth/drive.file`
   (only files the app creates or the user opens with it) → **Save**.
6. **Audience → Test users → Add users** → add your own Gmail address. While the app is in
   *Testing*, only listed users (max 100) can sign in and their sign-in lasts 7 days.
7. **Clients → Create client** → Application type **Desktop app** → name → **Create**.
   No redirect URI is entered: desktop clients accept `http://127.0.0.1:<any port>` automatically.
8. Copy the **Client ID** (ends in `.apps.googleusercontent.com`). Google also shows a client
   secret; for desktop apps it is not confidential, but keep it out of chat and source control —
   put it in the environment variable `SELENITE_TEST_GOOGLE_CLIENT_SECRET` on your machine.

### OneDrive (Microsoft Entra)

1. Open <https://entra.microsoft.com/> → **Applications → App registrations → New registration**.
   (A personal Microsoft account can register apps; it gets a default directory.)
2. Name: e.g. `Selenite dev`.
3. Supported account types: **Accounts in any organizational directory and personal Microsoft
   accounts**.
4. Redirect URI: platform **Public client/native (mobile & desktop)**, value `http://localhost`
   (Microsoft ignores the port for localhost redirects). **Register**.
5. **Authentication** → **Advanced settings → Allow public client flows: Yes** → **Save**.
6. **API permissions → Add a permission → Microsoft Graph → Delegated permissions** → add
   `Files.ReadWrite.AppFolder` and `offline_access` → **Add permissions**. No admin consent is
   needed; each user approves at sign-in.
7. Copy the **Application (client) ID** from **Overview** (a GUID). No secret is created.

### Hand-off

Set the IDs where the app reads them (development builds read environment variables):

```bash
setx SELENITE_TEST_GOOGLE_CLIENT_ID "<google client id>"
setx SELENITE_TEST_GOOGLE_CLIENT_SECRET "<google desktop client secret>"
setx SELENITE_TEST_ONEDRIVE_CLIENT_ID "<microsoft application id>"
```

## Part B — before public release

### Google

1. **Branding**: app logo, home page URL, privacy policy URL, terms URL (optional).
2. **Authorized domains**: add the domain hosting the home page and policy; verify ownership in
   Google Search Console.
3. **Audience → Publish app** (moves from *Testing* to *In production*).
4. **Verification centre → Submit for verification**. `drive.file` is a non-sensitive scope, so
   this is brand verification (name, logo, domain), usually a few days, with no security audit.
   Until it passes, users see an "unverified app" screen.
5. Put the production client ID in the release build config (the secret, if used, is shipped as a
   non-confidential value per Google's installed-app guidance).

### Microsoft

1. Nothing is required: any account can sign in once the app is registered as above.
2. Optional **Publisher verification** (Branding & properties) with a Microsoft Partner Network ID
   removes the "unverified" label on the consent screen.
3. Fill **Branding & properties**: logo, home page, terms and privacy URLs.

## Least-privilege scopes (locked decision ADR-004)

| Provider | Scope | Reach |
|----------|-------|-------|
| Google Drive | `drive.file` | Only files and folders the app created (or the user picked) |
| OneDrive | `Files.ReadWrite.AppFolder`, `offline_access` | Only `Apps/<app name>` in the user's OneDrive |

Never request full-drive scopes (`drive`, `Files.ReadWrite.All`).

# Third-party payloads

Not committed. `build.ps1` refuses to build without them, deliberately: a build that
downloads an executable from the internet and packages it unverified is the
supply-chain problem the bank's security review exists to find.

## `MicrosoftEdgeWebview2Setup.exe`

The WebView2 Evergreen bootstrapper (~2 MB). Windows 11 ships the runtime; Windows 10
does not (spec section 3).

Fetch from <https://developer.microsoft.com/microsoft-edge/webview2/>, then before
using it in a customer build:

```powershell
Get-AuthenticodeSignature .\MicrosoftEdgeWebview2Setup.exe | Format-List
Get-FileHash .\MicrosoftEdgeWebview2Setup.exe -Algorithm SHA256
```

The signature must name Microsoft Corporation. Record the SHA-256 in the release
notes so the exact payload that shipped can be identified later.

See OPEN-5 in `../README.md`: the air-gapped fixed-version runtime path is not
decided, and nothing in this directory presumes an answer.

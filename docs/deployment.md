# Deploying the Sentinel endpoint agent

This guide is for the IT lead deploying Sentinel to a collections floor. It covers what
Windows builds are supported and what changes on the older ones, how audio devices have
to be configured, the WebView2 runtime, and the endpoint-protection work that has to
start before anything else.

Read the section on EDR allowlisting first if you are planning a schedule. It is the
item most likely to move your dates.

**Where this stands, so you can plan honestly.** The MSI is written — a complete WiX
package with the tier gate, the service registration, the ACLs and the WebView2 handling
described below — and it has not yet been built or signed. No installer has been produced
from this repository, nothing has been installed on a desktop, and the Windows-only
capture code has never run outside a compiler. Everything below describes how the product
is designed and packaged to behave, which is the right thing to plan a Phase 0 around; it
is not a report from a machine. Ask for a signed build and a pilot image before you commit
to dates that depend on one.

## Windows support matrix

| Tier | Operating system | How audio is captured | What you get |
|---|---|---|---|
| A | Windows 11 (build 22000 or later), Windows Server 2022 or later | Process loopback | Full support |
| B | Windows 10 1903 (18362) through 22H2 (19045) | Endpoint loopback from a pinned device | Degraded support — see below |
| C | Windows 8.1, Windows 10 earlier than 1903, 32-bit x86, ARM64 | Not possible | The installer blocks |

The dividing line is a specific Windows API. Process loopback —
`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, reached through
`ActivateAudioInterfaceAsync` — captures the audio of one named process and nothing
else. It requires Windows 11 or Server 2022. Windows 10 client builds top out at 19045
and do not have it, at any patch level.

If you have seen the threshold quoted as "build 20348 or later", that figure is the
Windows Server 2022 build number. It is not a Windows 10 build, and a Windows 10 machine
will never reach it. Treat every Windows 10 desktop on the floor as tier B.

The agent detects the tier at install time and again on every service start, because an
in-place feature update changes the answer. It reports `capture_tier` and `os_build` in
every heartbeat, and the portal's fleet view shows the tier distribution across your
estate so you can see what a Windows 11 upgrade would buy you.

## What tier B means for you

On tier A, Sentinel records the softphone and only the softphone. On tier B, that API
does not exist, so the agent records the audio stream going to one audio device instead.

That means: on a tier B machine, whatever is being played to the agent's headset is
captured. Music, a Teams call, a YouTube video in another window, Windows notification
sounds. Not what is played through the laptop speakers, and not what other applications
send to other devices — but everything that reaches the pinned headset.

Someone in your organisation will ask whether Sentinel records agents' music. On tier B
the honest answer is that the audio stream does reach the agent, and there are two
mitigations, both of which are required and both of which are built:

**Device pinning.** The agent captures only from a specific audio endpoint, identified by
its container ID, that an administrator has pinned in tenant policy. It never captures
from the system default device. If the pinned headset is not present, capture does not
start at all — the widget says "headset not detected" and the state is reported in the
heartbeat rather than silently falling back to something else.

**Foreign-audio suppression.** The agent watches the softphone's audio session state
alongside the audio itself. When there is audio energy above the voice-detection
threshold but the softphone's session is inactive, that audio is not call audio. Those
segments are marked `foreign` in the frame header. The server stores them so that a
reviewer can be shown exactly what was set aside, and never sends them to speech
recognition. The suppressor allows a short grace window on each side of a session state
change, because the state notification leads and lags the actual audio, and when the
signals are ambiguous it errs toward marking audio foreign: losing a few seconds of
transcript on one call is a smaller problem than transcribing an agent's music.

The widget displays the capture tier, and tier B shows a distinct indicator, so the
agent can see which mode their machine is in.

## Pinning a headset by container ID

Device pinning is tenant configuration, delivered to the agent by `GET /v1/policy`. You
provide the container ID of the headset model your floor uses. The friendly name can be
supplied as a fallback, but the container ID is what the match is made on.

Use the container ID, not the endpoint ID. The endpoint ID changes when a headset is
unplugged and plugged into a different USB port; the container ID does not. Agents
unplug headsets constantly — during breaks, at shift change, when the cable snags — and
a device identity that changes on replug means capture silently stops for the rest of
the shift.

Practically this means you need to know which headset models are in use before you
deploy, and you need one policy entry per model. Mixed estates are normal; the policy
takes a list. This is one of the items on the Phase 0 checklist for exactly this reason.

## WebView2 runtime

The agent's widget is rendered with WebView2. The runtime ships as part of Windows 11.
It does not ship with Windows 10, which means your tier B machines are also the machines
that need the runtime installed.

The package as written bundles the Evergreen bootstrapper, which fetches and installs the
runtime on first run. That requires outbound access to Microsoft's distribution endpoints
from the desktop, at install time. Two details worth knowing before you test it: the
installer looks for an existing per-machine runtime first and skips the bootstrapper if
one is already present, and if the bootstrapper does fail the install is allowed to
continue rather than rolling back — you get capture without a widget, which is a better
outcome than a floor with neither, and the missing runtime is reported in the agent's
heartbeat so it shows up in the fleet view rather than as an agent complaint.

If your floor is air-gapped or has strict egress filtering, the Evergreen bootstrapper
will not work and a fixed-version runtime has to be bundled instead. That branch has not
been built. Which of the two Sentinel ships is still an open decision (OPEN-5 in
`docs/open-decisions.md`) and the current package should be read as the unresolved default
rather than as the answer; if your deployment cannot reach the internet at install time,
say so during Phase 0 so the decision is made in your favour rather than discovered during
the pilot.

## Windows 10 end of support

Windows 10 reached end of support in October 2025. Any tier B fleet is running on
Extended Security Updates.

This has three consequences worth putting in front of whoever signs off the deployment.
Your ESU coverage has an end date, and it is worth knowing whether it falls before or
after your Sentinel contract term. Your bank client's security reviewer will ask about it
independently of Sentinel, and the answer is easier if you have already documented it.
And the capture quality difference between tier A and tier B is real, so a Windows 11
upgrade programme that you were already considering has an additional argument behind it.

Sentinel supports tier B properly and will keep doing so. This is a note about your
estate, not a threat to withdraw support.

## The endpoint protection conversation

Start this in Phase 0. Not Phase 2, not when the pilot machines are being imaged. Phase
0.

Here is what the Sentinel agent does, described the way a detection engine sees it. It
runs a background process that captures audio from the microphone and from the system's
audio output. It maintains an outbound network connection and continuously uploads that
audio to a remote server. It runs a window that is not in the taskbar. It is launched by
a SYSTEM service that restarts it if it exits. It writes an encrypted local database
that the user cannot read.

That is, feature for feature, the behavioural signature of a keylogger with audio
capture. Any competent endpoint protection product will flag it, and the better the
product, the more confidently it will flag it. This is not a sign that something is
wrong with the agent; it is the correct output of a detection engine looking at those
behaviours. Code signing with an EV certificate helps with reputation-based heuristics
and does not, on its own, prevent a behavioural detection.

What this means for your plan:

**It takes calendar time, not engineering time.** Getting an allowlist entry through a
security vendor is a ticket, a review, and often a conversation with a named engineer.
Two to six weeks is normal. None of that time is compressible by working harder on the
software.

**You need to know your vendor and your policy owner before you start.** Which product,
which console, who administers it, and who is allowed to approve an exclusion. If the
console is managed by your bank client rather than by you, that is a longer conversation
and it needs to start earlier.

**Ask for a behavioural exclusion scoped to the signed binaries**, by publisher and path,
rather than a blanket folder exclusion. A blanket exclusion is easier to get and worse
for everyone, and your bank's security reviewer may well object to it later.

**Test on a machine with the production policy applied**, not a machine with protection
relaxed for testing. A pilot that passes on an unprotected image tells you nothing.

The Phase 2 acceptance gate includes zero EDR quarantines across five full shifts on ten
machines. That gate is not passable if the allowlisting conversation starts when the
pilot does.

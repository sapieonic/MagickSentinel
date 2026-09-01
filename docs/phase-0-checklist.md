# Phase 0 discovery checklist

Phase 0 is two weeks of finding out whether capturing audio at the Windows endpoint
will work on this particular floor. It exits with a go/no-go on the endpoint approach.

Work through this with the customer's IT lead and operations manager in the room. Most
of it cannot be answered from a spreadsheet someone emails you, and several answers
change the architecture rather than the configuration.

---

## Stop-and-escalate criterion

**Answer this before anything else on the list.**

> **If any part of the floor runs Citrix, VMware Horizon, Azure Virtual Desktop or any
> other VDI or thin-client environment, or if agents use hardware SIP handsets rather
> than a softphone on the PC — stop. Do not proceed with the rest of Phase 0. Escalate.**

The reason is not that it would be difficult. It is that it does not work.

On a hardware SIP handset, the audio path runs from the phone to the network. It never
enters the PC. There is no Windows audio API that can capture audio that was never
rendered on that machine, and no amount of engineering changes that. The endpoint
capture approach is simply the wrong architecture for that floor, and the answer is a
different capture point — network-side or a hardware interposer — which is a different
product decision, not a configuration change.

On VDI, audio does reach the endpoint, but it reaches it after being compressed by the
remoting protocol's audio redirection and shifted in time. Both matter. Compressed,
redirected audio degrades speech recognition, and it degrades it worst on exactly the
tokens the product depends on — amounts, dates and account numbers. A promise-to-pay of
₹15,000 transcribed as ₹50,000 destroys trust in every other number the system reports.
Latency shift breaks the alignment between the two channels, which is what makes speaker
attribution exact.

A partial answer is still a stop. "Most of the floor is on physical desktops, one team
of thirty is on Horizon" means the architecture needs revisiting for that team before you
commit to a rollout plan that assumes it will work.

Record the answer in writing, from someone who administers the desktops, not from
someone who uses one.

---

## Fleet OS build census

You need the actual build number for every machine that will run the agent, not the
marketing name. `winver` gives it, so does `Get-ComputerInfo`, and your management tool
(SCCM, Intune) can export it for the whole estate in one query — ask for that export.

What you are producing is a count by build, mapped to the tiers in
`docs/deployment.md`:

- Windows 11 build 22000 or later, or Server 2022 or later — tier A, full support.
- Windows 10 build 18362 through 19045 — tier B, degraded support.
- Anything older, or 32-bit x86, or ARM64 — tier C, the installer blocks.

Three things to establish beyond the counts. How many tier C machines are there, and
what is the plan for them, given that they cannot be monitored at all? Is there an
upgrade programme already scheduled that would move machines between tiers during the
pilot? And are the machines uniform, or is there a long tail of one-off builds that will
each need to be looked at?

Also note that tier B means the machine is past Windows 10 end of support and is running
on ESU. Confirm the ESU coverage and its end date while you are asking.

## Softphone identification and PID resolution

Which softphone application do agents actually use for calls? Get the executable name,
the vendor, and the version. If the dialer is supplied by the bank client, get it from
the bank client's documentation rather than from the desktop image, because the desktop
image may lag.

Then establish how the running process is found. The agent needs to resolve a process ID
to attach process loopback on tier A and to track the audio session state on both tiers.
The complications to look for: does the softphone run as a child of a launcher, so the
process holding the audio is not the one with the recognisable name? Does it spawn a
separate media process per call? Does the process name change between versions? Is there
more than one softphone in use across teams?

Confirm the answer by watching a real call on a real desktop with Task Manager open. This
is the item most often answered wrongly from documentation.

While you are there, capture the UI Automation selectors for the account or loan
reference in the dialer window, if the dialer exposes one. This is best-effort metadata —
when it is absent the server reconciles against the dialer's CDR export instead — but one
worked example is needed to prove the mechanism (OPEN-8).

## VDI and thin client check

Covered by the stop-and-escalate criterion above, and repeated here because it must be
an explicit item on the list with an explicit answer, not an assumption.

For every team on the floor, record: physical desktop, laptop, VDI (which product), or
thin client. Ask specifically about work-from-home arrangements, which frequently run on
a different stack from the office floor and are frequently forgotten in the survey.

## Headset models

List every headset model in use, with counts, and get the container ID for each.

Device pinning matches on container ID, so this list becomes tenant policy. A model you
miss is a group of agents whose capture never starts.

Ask also about the headset lifecycle. Are headsets assigned to an agent or to a desk? Do
agents take them home? Is there a spares pool, and if so what is in it? A spares pool
full of a model that is not in your policy will produce a slow trickle of
"headset not detected" reports that nobody can explain.

## Identity provider inventory

Does the customer run Microsoft Entra ID, or another SAML or OIDC identity provider?
This determines whether agents sign in with corporate credentials via SSO or whether
Sentinel has to own an email-and-password flow for the whole floor (OPEN-2).

Establish: the provider and tenant, who administers it, whether agents already have
individual accounts in it or share credentials, whether MFA is enforced and by what
method, and what the joiner-mover-leaver process is. Shared or generic agent accounts are
worth flagging early — they undermine per-agent attribution, which is the basis of every
metric the product produces.

Ask about shift patterns at the same time. Collections floors commonly run two or three
shifts on the same desktops, and Sentinel treats that as a first-class case: no
signed-in user means no capture, and sign-out flushes the spool before clearing tokens.
You need to know the shift boundaries to interpret the coverage numbers.

## Language mix

Which languages are actually spoken on calls, in what proportion, and by which teams?

Expect code-mixing rather than clean single-language calls — Hinglish is the norm, and
Telugu, Tamil and Marathi appear alongside it. Ask for the mix per team rather than for
the floor as a whole, because it varies by the portfolio a team works.

This drives the speech recognition evaluation, which needs a hand-labelled set of 200
real calls per language before Phase 3 can exit. Knowing the mix in Phase 0 is what makes
that set collectable on time.

## Network egress rules

What can the desktops reach, and through what?

Specifically: can they open an outbound WebSocket over TLS to the Sentinel gateway, and
to which hostname and port? Is there a proxy, and does it require authentication? Is
there TLS interception on the egress path — this matters, because the device
authenticates with mutual TLS and an intercepting proxy will break it. What is the
available upstream bandwidth per desktop and in aggregate; at roughly 6 KB/s per agent
for both channels, a 200-seat floor needs about 1.2 MB/s sustained upstream, which is
modest but not nothing if the floor's uplink is already saturated.

Confirm whether the desktops can reach Microsoft's WebView2 distribution endpoints at
install time, which decides the runtime question in `docs/deployment.md`.

Get these rules from whoever administers the firewall, and get the change request raised
in Phase 0 rather than discovered in Phase 1.

## Endpoint protection

Not on the original list, and it belongs here anyway, because it is the item with the
longest lead time.

Identify the endpoint protection product, the console owner, and the person who can
approve a behavioural exclusion. Open the conversation now. The full reasoning is in
`docs/deployment.md`; the short version is that the agent's behaviour is indistinguishable
from a keylogger to any competent detection engine, allowlisting takes weeks of calendar
time, and the Phase 2 acceptance gate requires zero quarantines.

## Written confirmation of who approves the install

Get a named person, in writing, who is authorised to approve installing the agent on
agent desktops.

This is not bureaucracy. Sentinel records audio at people's workstations. The question
"who agreed to this" will be asked — by the works council or its local equivalent, by an
agent, by the bank client's compliance function, or by a regulator — and the answer
needs to be a name and a date rather than a recollection of a meeting.

Confirm at the same time who is responsible for telling the agents, and what they will be
told. Agent cooperation is a hard dependency for this product: agents will attempt to
disable software that scores them, and how the deployment is introduced materially
affects how much of that you get. That conversation is easier before the software
arrives.

---

## Exit

Phase 0 exits with a documented go/no-go on the endpoint capture approach.

Go requires: no VDI, no thin clients and no hardware SIP handsets anywhere in scope; a
tier census with a plan for the tier C machines; a softphone whose process can be
resolved, verified on a real desktop; a headset list with container IDs; an identity
provider decision; a language mix; egress confirmed or a change request raised; the EDR
conversation started with a named owner; and the install approver named in writing.

Anything unresolved goes on the open-decisions list rather than into an assumption. See
`docs/open-decisions.md`.

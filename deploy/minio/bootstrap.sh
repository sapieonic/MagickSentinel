#!/bin/sh
# Creates the audio bucket and the least-privileged user the pipeline uses to reach it.
#
# Runs once, in the `minio-init` service, after MinIO reports healthy. `/bin/sh`
# because the minio/mc image is BusyBox-based and has no bash.
#
# ==============================================================================
# WHY THERE IS A SECOND SET OF CREDENTIALS
#
# MINIO_ROOT_USER can create and delete buckets and read every object in them. Handing
# that to an application container means a compromised worker can delete every
# recorded call in the deployment — which, for a compliance product whose whole claim
# is "100% of calls are monitored and retained", is the most damaging single thing an
# attacker could do. It is also indistinguishable from a retention job bug.
#
# So the root credential stays in this one-shot init container and the application gets
# a policy that can read and write objects under the audio prefix and cannot delete the
# bucket or change the policy.
#
# ==============================================================================
# WHAT THIS DOES NOT DO, AND WHY
#
# **No object lock / WORM, no versioning.** Both are strong arguments for a compliance
# product and both interact directly with OPEN-6, which is undecided: retention periods
# for audio and transcripts have never been checked against a real requirement, and
# `tenants.audio_retention_days` = 30 is documented as a placeholder. Object lock is
# the one storage setting that cannot be undone after the fact — a locked object cannot
# be deleted before its retention expires, by anyone, including us. Turning it on
# before the retention period is decided would make a placeholder number permanent.
# When OPEN-6 is settled, this is where compliance-mode object lock belongs.
#
# **No lifecycle expiry rule.** The purge is `retention.py`'s job, per tenant, with an
# audit entry per batch (spec 6.6 requires the audit entry). A bucket lifecycle rule
# would delete audio on a schedule with no audit trail and no per-tenant period, which
# is a different behaviour wearing the same name.
#
# **No encryption-at-rest configuration.** Left to the storage layer deliberately: on
# MinIO that is KMS configuration, and on the real deployment it is S3 SSE with a key
# whose residency is part of OPEN-4. Setting a default here would imply an answer.
set -eu

MINIO_ALIAS=local
: "${MINIO_ENDPOINT:?MINIO_ENDPOINT is required}"
: "${MINIO_ROOT_USER:?MINIO_ROOT_USER is required}"
: "${MINIO_ROOT_PASSWORD:?MINIO_ROOT_PASSWORD is required}"
: "${SENTINEL_S3_BUCKET:?SENTINEL_S3_BUCKET is required}"
: "${SENTINEL_S3_ACCESS_KEY:?SENTINEL_S3_ACCESS_KEY is required}"
: "${SENTINEL_S3_SECRET_KEY:?SENTINEL_S3_SECRET_KEY is required}"

log() { printf 'minio-init: %s\n' "$*" >&2; }

log "pointing mc at $MINIO_ENDPOINT"
mc alias set "$MINIO_ALIAS" "$MINIO_ENDPOINT" "$MINIO_ROOT_USER" "$MINIO_ROOT_PASSWORD" >/dev/null

# `mc ready` blocks until the deployment can serve requests. compose's
# `depends_on: service_healthy` already covers this; the second check costs nothing and
# turns a race into a wait.
mc ready "$MINIO_ALIAS"

if mc ls "$MINIO_ALIAS/$SENTINEL_S3_BUCKET" >/dev/null 2>&1; then
  log "bucket $SENTINEL_S3_BUCKET already exists"
else
  # The region is passed even locally. It is not functionally required by MinIO, but
  # `blob.SegmentKey` keys objects as audio/{tenant}/{day}/{call}/{channel}/... and the
  # region a bucket was created in is the one fact about an object store that cannot be
  # changed later. OPEN-4's working assumption is India-only (ap-south-1); making the
  # local stack agree with it means nobody develops against us-east-1 defaults and then
  # discovers the assumption at deployment time. See deploy/.env.example.
  log "creating bucket $SENTINEL_S3_BUCKET in region ${SENTINEL_S3_REGION:-ap-south-1}"
  mc mb --region "${SENTINEL_S3_REGION:-ap-south-1}" "$MINIO_ALIAS/$SENTINEL_S3_BUCKET"
fi

# Public access explicitly denied rather than left at the default. It IS the default —
# stating it means a later `mc anonymous set download` by someone debugging a 403 shows
# up as a change to a deliberate setting rather than a change to an unexamined one.
mc anonymous set none "$MINIO_ALIAS/$SENTINEL_S3_BUCKET"

# ---------------------------------------------------------------- application policy
#
# Read and write objects; list the bucket; no DeleteBucket, no PutBucketPolicy, no
# PutBucketVersioning. DeleteObject IS granted, because retention.py has to delete each
# audio object before its database row — that ordering is deliberate, so a failed
# object delete leaves the row for the next run to retry rather than orphaning audio no
# sweep can ever find again.
POLICY_FILE=/tmp/sentinel-app-policy.json
cat > "$POLICY_FILE" <<POLICY
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["s3:GetBucketLocation", "s3:ListBucket"],
      "Resource": ["arn:aws:s3:::${SENTINEL_S3_BUCKET}"]
    },
    {
      "Effect": "Allow",
      "Action": ["s3:GetObject", "s3:PutObject", "s3:DeleteObject", "s3:AbortMultipartUpload"],
      "Resource": ["arn:aws:s3:::${SENTINEL_S3_BUCKET}/*"]
    }
  ]
}
POLICY

mc admin policy create "$MINIO_ALIAS" sentinel-app "$POLICY_FILE" 2>/dev/null \
  || mc admin policy update "$MINIO_ALIAS" sentinel-app "$POLICY_FILE"

if mc admin user info "$MINIO_ALIAS" "$SENTINEL_S3_ACCESS_KEY" >/dev/null 2>&1; then
  log "service user $SENTINEL_S3_ACCESS_KEY already exists"
else
  mc admin user add "$MINIO_ALIAS" "$SENTINEL_S3_ACCESS_KEY" "$SENTINEL_S3_SECRET_KEY"
fi
mc admin policy attach "$MINIO_ALIAS" sentinel-app --user "$SENTINEL_S3_ACCESS_KEY" 2>/dev/null || true

rm -f "$POLICY_FILE"
log "done: bucket ${SENTINEL_S3_BUCKET}, service user ${SENTINEL_S3_ACCESS_KEY} with the sentinel-app policy"

-- Default rule set, installed for every new tenant and fully overridable.
--
-- Term lists here are a starting point sized for a pilot, deliberately narrow so the
-- first flags a compliance reviewer sees are obviously right. Widening them is a
-- tenant configuration change (PUT /v1/admin/rules), which creates a new version.
--
-- SCRIPT CONTRACT: every non-English list carries the term in BOTH the native script
-- and a romanisation, because the ASR output for one Hinglish call is not
-- consistently in one script and the pipeline does not pin a language. Matching folds
-- each side by the script of the term itself (see compliance/engine.py: `romanised`
-- collapses inflection and vowel-length variance in transliterations, `indic` strips
-- trailing vowel signs), so the base form is enough and case endings need no entry.
-- Genuinely different spellings — aspirated or not, bhikhari/bhikari — do need one.
--
-- THE VOCABULARY BELOW HAS NOT BEEN REVIEWED BY A NATIVE SPEAKER OF EVERY LANGUAGE
-- IT COVERS, and it has never been measured against real call audio. Treat coverage
-- as unproven until Phase 0 puts it in front of the floor's own QA team. Under-firing
-- is silent: a Hindi call the rules cannot read looks exactly like a clean one.
--
-- Re-seeding an existing database: this migration is the source of truth for the
-- defaults and for `load_default_rule_set()`. A database that applied an earlier
-- version of it needs
--   UPDATE default_rule_set SET definition = ... ;  -- the JSON below
-- and a new rule_sets version per tenant; nothing re-reads it automatically.

BEGIN;

CREATE TABLE default_rule_set (
  version    int PRIMARY KEY,
  definition jsonb NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);

INSERT INTO default_rule_set (version, definition) VALUES (1, $json$
{
  "call_hours": { "start": "08:00", "end": "19:00", "timezone": "Asia/Kolkata" },
  "judge_sample_pct": 5,
  "rules": [
    {
      "rule_id": "abusive_language",
      "enabled": true, "severity": "critical", "judge": true,
      "params": {
        "channel": "near",
        "terms": {
          "en": ["bastard", "idiot", "stupid", "shameless", "useless fellow", "beggar", "thief", "fraudster", "cheat"],
          "hi": ["kamine", "kutte", "besharam", "nikamma", "chor", "bhikhari", "badtameez",
                 "कमीने", "कुत्ते", "बेशर्म", "निकम्मा", "चोर", "भिखारी", "बदतमीज़"],
          "ta": ["muttal", "vekkam illama", "முட்டாள்", "வெக்கம் இல்லாம"],
          "te": ["dongodu", "dongodulu", "siggu leni", "దొంగోడు", "దొంగోళ్ళు", "సిగ్గు లేని"],
          "mr": ["nalayak", "besharam", "नालायक", "बेशर्म"]
        }
      }
    },
    {
      "rule_id": "threat_of_violence",
      "enabled": true, "severity": "critical", "judge": true,
      "params": {
        "channel": "near",
        "terms": {
          "en": ["break your legs", "send goons", "come to your house and", "teach you a lesson", "you will regret"],
          "hi": ["haath pair tod", "ghar pe aake dekh", "gunde bhej", "dekh lunga tujhe",
                 "हाथ पैर तोड़", "घर पे आके देख", "गुंडे भेज", "देख लूंगा तुझे"]
        },
        "patterns": [
          "(?i)\\b(i|we)\\s+(will|'ll|shall)\\s+(come|send|bring)\\b[^.?!]{0,40}\\b(house|home|office|address)\\b"
        ]
      }
    },
    {
      "rule_id": "false_legal_threat",
      "enabled": true, "severity": "critical", "judge": true,
      "params": {
        "channel": "near",
        "terms": {
          "en": ["you will be arrested", "police case", "criminal case", "non-bailable", "warrant", "jail", "fir against you", "cbi"],
          "hi": ["giraftaar", "giriftaar", "police case", "jail bhej", "warrant nikal", "criminal case",
                 "गिरफ्तार", "पुलिस केस", "जेल भेज", "वारंट निकल", "क्रिमिनल केस"]
        },
        "patterns": [
          "(?i)\\b(arrest|jail|police|warrant|criminal\\s+case|f\\.?i\\.?r\\.?)\\b[^.?!]{0,60}\\b(tomorrow|today|within|by)\\b"
        ]
      }
    },
    {
      "rule_id": "false_seizure_threat",
      "enabled": true, "severity": "high", "judge": true,
      "params": {
        "channel": "near",
        "terms": {
          "en": ["seize your", "take away your car", "attach your property", "confiscate", "repossess today", "lock your house"],
          "hi": ["gaadi utha lenge", "makaan seal", "saman utha lenge", "kabza kar lenge",
                 "गाड़ी उठा लेंगे", "मकान सील", "सामान उठा लेंगे", "कब्ज़ा कर लेंगे"]
        }
      }
    },
    {
      "rule_id": "third_party_disclosure",
      "enabled": true, "severity": "critical", "judge": true,
      "params": {
        "channel": "near",
        "comment": "Fires when the far speaker has denied being the borrower and the near speaker still discloses balance, amount or account details.",
        "denial_terms": {
          "en": ["wrong number", "he is not here", "she is not here", "i am not", "who is this", "neighbour", "this is not his number"],
          "hi": ["galat number", "wo yahan nahi", "main nahi hoon", "kaun bol raha", "padosi",
                 "गलत नंबर", "वो यहाँ नहीं", "मैं नहीं हूँ", "कौन बोल रहा", "पड़ोसी"]
        },
        "disclosure_terms": {
          "en": ["outstanding", "loan amount", "due amount", "emi", "overdue", "balance is", "account number"],
          "hi": ["bakaya", "loan ki rakam", "emi", "kist", "khata number",
                 "बकाया", "लोन की रकम", "ईएमआई", "किस्त", "खाता नंबर"]
        },
        "window_ms": 120000
      }
    },
    {
      "rule_id": "outside_call_hours",
      "enabled": true, "severity": "high", "judge": false,
      "params": { "comment": "Evaluated against calls.started_at in the tenant timezone. Structural: no transcript needed." }
    },
    {
      "rule_id": "missing_identification",
      "enabled": true, "severity": "medium", "judge": false,
      "params": {
        "channel": "near",
        "window_ms": 30000,
        "requires": ["agent_name", "agency_name"],
        "agency_terms": {
          "en": ["calling from", "on behalf of", "recovery agency", "collections department"],
          "hi": ["se bol raha", "se bol rahi", "ki taraf se",
                 "से बोल रहा", "से बोल रही", "की तरफ से"]
        }
      }
    },
    {
      "rule_id": "no_purpose_disclosure",
      "enabled": true, "severity": "medium", "judge": false,
      "params": {
        "channel": "near",
        "window_ms": 60000,
        "terms": {
          "en": ["regarding your loan", "about your outstanding", "payment due", "overdue emi", "loan account", "regarding the credit card"],
          "hi": ["aapke loan ke baare", "bakaya rakam", "emi ke baare", "payment ke liye",
                 "आपके लोन के बारे", "बकाया रकम", "ईएमआई के बारे", "पेमेंट के लिए"]
        }
      }
    },
    {
      "rule_id": "excessive_interruption",
      "enabled": true, "severity": "low", "judge": false,
      "params": { "threshold": 12, "comment": "Interruption count from analyses.interruptions." }
    },
    {
      "rule_id": "repeat_contact",
      "enabled": true, "severity": "medium", "judge": false,
      "params": { "threshold": 3, "window_ms": 86400000, "comment": "> N calls to the same account_ref in 24 h." }
    }
  ]
}
$json$::jsonb);

-- Give every tenant that already exists version 1, active.
INSERT INTO rule_sets (tenant_id, version, definition, active, created_by)
SELECT t.id, 1, d.definition, true, 'system'
FROM tenants t CROSS JOIN default_rule_set d
WHERE d.version = 1
ON CONFLICT (tenant_id, version) DO NOTHING;

COMMIT;

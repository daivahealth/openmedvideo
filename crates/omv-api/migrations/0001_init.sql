-- Catalog schema. The DICOM in PACS is the source of truth; everything here
-- describes regenerable video renditions and who watched them.

CREATE TABLE IF NOT EXISTS studies (
    study_uid        TEXT PRIMARY KEY,          -- DICOM StudyInstanceUID
    orthanc_id       TEXT NOT NULL,             -- Orthanc internal study id
    description      TEXT NOT NULL DEFAULT '',
    patient_ref      TEXT NOT NULL DEFAULT '',  -- coded reference, never demographics
    modalities       TEXT NOT NULL DEFAULT '',
    status           TEXT NOT NULL DEFAULT 'queued',
    error            TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS renditions (
    id               BIGSERIAL PRIMARY KEY,
    study_uid        TEXT NOT NULL REFERENCES studies(study_uid) ON DELETE CASCADE,
    series_uid       TEXT NOT NULL,
    series_description TEXT NOT NULL DEFAULT '',
    modality         TEXT NOT NULL,
    preset           TEXT NOT NULL,
    preset_label     TEXT NOT NULL,
    playlist         TEXT NOT NULL,             -- storage key relative to study prefix
    frames           INTEGER NOT NULL,
    fps              DOUBLE PRECISION NOT NULL,
    UNIQUE (study_uid, series_uid, preset)
);

-- Append-only audit trail (§7.3): who viewed what, via which client app.
CREATE TABLE IF NOT EXISTS audit_events (
    id               BIGSERIAL PRIMARY KEY,
    practitioner     TEXT NOT NULL,
    client_app       TEXT NOT NULL,
    study_uid        TEXT NOT NULL,
    action           TEXT NOT NULL,             -- view | export | denied
    at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    detail           TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_audit_study ON audit_events(study_uid, at);

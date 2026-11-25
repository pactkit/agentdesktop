-- Per authenticated principal serialization for concurrent enrollment.
-- Rows carry no credentials; issuer/subject are already stored in devices.
CREATE TABLE enrollment_subject_locks (
    issuer TEXT NOT NULL,
    subject TEXT NOT NULL,
    PRIMARY KEY (issuer, subject)
);

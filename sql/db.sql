CREATE TYPE instance_status AS ENUM ('live', 'dead');

CREATE TABLE IF NOT EXISTS instances (
	id varchar(64) PRIMARY KEY,
	last_at timestamptz NOT NULL DEFAULT NOW(),
	status instance_status NOT NULL DEFAULT 'live'
);

CREATE TABLE IF NOT EXISTS jobs (
	id bigint PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY,
	created_at timestamptz NOT NULL DEFAULT NOW(),
	protocol varchar(16) NOT NULL,
	meta jsonb NOT NULL,
	headers jsonb NULL,
	body BYTEA NULL
);

CREATE TABLE IF NOT EXISTS scheduled (
	id bigint NOT NULL,
	at bigint NOT NULL,
	retry int NOT NULL DEFAULT 0,
	
	CONSTRAINT pk_scheduled PRIMARY KEY (id),
	CONSTRAINT fk_jobs_id FOREIGN KEY (id)
        REFERENCES jobs (id) MATCH SIMPLE
        ON UPDATE NO ACTION
        ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS enqueued (
	id bigint NOT NULL,
	retry int NOT NULL DEFAULT 0,
	instance_id varchar(64) NULL,
	lock_at timestamptz NULL,

	CONSTRAINT pk_enqueued PRIMARY KEY (id),
	CONSTRAINT fk_jobs_id FOREIGN KEY (id)
        REFERENCES jobs (id) MATCH SIMPLE
        ON UPDATE NO ACTION
        ON DELETE CASCADE
);

CREATE TYPE processed_status AS ENUM ('completed', 'failed', 'cancelled');

CREATE TABLE IF NOT EXISTS processed (
	id bigint NOT NULL,
	retry int NOT NULL DEFAULT 0,
	instance_id varchar(64) NOT NULL,
	at timestamptz NOT NULL DEFAULT NOW(),
	status processed_status NOT NULL,
	meta jsonb NOT NULL,
	headers jsonb NULL,
	body BYTEA NULL,

	CONSTRAINT fk_jobs_id FOREIGN KEY (id)
        REFERENCES jobs (id) MATCH SIMPLE
        ON UPDATE NO ACTION
        ON DELETE CASCADE
);

CREATE TYPE history_status AS ENUM ('scheduled', 'enqueued', 'assigned', 'retried', 'completed', 'failed', 'cancelled');

CREATE TABLE IF NOT EXISTS history (
	id bigint NOT NULL,
	retry int NOT NULL DEFAULT 0,
	status history_status NOT NULL,
	instance_id varchar(64) NOT NULL,
	at timestamptz NOT NULL DEFAULT NOW(),

	CONSTRAINT fk_jobs_id FOREIGN KEY (id)
        REFERENCES jobs (id) MATCH SIMPLE
        ON UPDATE NO ACTION
        ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS ix_enqueued_retry_id
    ON enqueued USING btree (retry ASC NULLS LAST, id ASC NULLS LAST)
    WHERE lock_at IS NULL;
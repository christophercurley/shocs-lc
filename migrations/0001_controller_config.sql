-- PH3-0B: first durable SHOCS-LC configuration schema.
-- Runtime observations (IP address, last seen, HSBK polls, etc.) intentionally
-- remain in memory and do not belong in these configuration tables.

CREATE TABLE lights (
    lifx_id         TEXT PRIMARY KEY,
    device_label    TEXT,
    friendly_name   TEXT,
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    mode            TEXT NOT NULL DEFAULT 'custom',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT lights_lifx_id_format
        CHECK (lifx_id ~ '^[0-9a-f]{16}$'),
    CONSTRAINT lights_mode_valid
        CHECK (mode IN ('custom', 'test')),
    CONSTRAINT lights_friendly_name_not_blank
        CHECK (friendly_name IS NULL OR length(btrim(friendly_name)) > 0)
);

CREATE TABLE light_groups (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name        TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT light_groups_name_not_blank
        CHECK (length(btrim(name)) > 0)
);

-- Group names are unique without making case part of identity.
CREATE UNIQUE INDEX light_groups_name_ci_unique
    ON light_groups (lower(name));

CREATE TABLE light_group_members (
    group_id    BIGINT NOT NULL REFERENCES light_groups(id) ON DELETE CASCADE,
    lifx_id     TEXT NOT NULL REFERENCES lights(lifx_id) ON DELETE CASCADE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (group_id, lifx_id)
);

CREATE INDEX light_group_members_lifx_id_idx
    ON light_group_members (lifx_id);

-- PH3-A1.1: make SHOCS friendly names safe, normalized, and unique.
--
-- Friendly names are SHOCS-owned human identifiers, unlike device_label,
-- which is observed metadata from the physical LIFX device.

-- Normalize any pre-existing values before constraints are added.
UPDATE lights
SET friendly_name = NULLIF(btrim(friendly_name), '')
WHERE friendly_name IS NOT NULL;

-- Fail loudly if legacy data would make the new rules ambiguous.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM lights
        WHERE friendly_name IS NOT NULL
          AND octet_length(friendly_name) > 31
    ) THEN
        RAISE EXCEPTION
            'cannot apply friendly-name guardrails: an existing friendly_name exceeds 31 UTF-8 bytes';
    END IF;

    IF EXISTS (
        SELECT lower(friendly_name)
        FROM lights
        WHERE friendly_name IS NOT NULL
        GROUP BY lower(friendly_name)
        HAVING count(*) > 1
    ) THEN
        RAISE EXCEPTION
            'cannot apply friendly-name guardrails: duplicate friendly names exist after normalization';
    END IF;
END
$$;

ALTER TABLE lights
    ADD CONSTRAINT lights_friendly_name_valid
    CHECK (
        friendly_name IS NULL
        OR (
            friendly_name = btrim(friendly_name)
            AND friendly_name <> ''
            AND octet_length(friendly_name) <= 31
        )
    );

-- PostgreSQL unique indexes naturally allow multiple NULLs.
-- lower() makes SHOCS names case-insensitively unique:
-- "Driveway", "driveway", and "DRIVEWAY" are the same logical name.
CREATE UNIQUE INDEX lights_friendly_name_unique
    ON lights (lower(friendly_name))
    WHERE friendly_name IS NOT NULL;

-- PH3-A2: harden SHOCS light-group names before exposing group management.
-- Group tables themselves were created in migration 0001.

UPDATE light_groups
SET name = btrim(name)
WHERE name IS DISTINCT FROM btrim(name);

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM light_groups
        WHERE octet_length(name) > 64
    ) THEN
        RAISE EXCEPTION
            'cannot apply group-management guardrails: an existing group name exceeds 64 UTF-8 bytes';
    END IF;

    IF EXISTS (
        SELECT lower(name)
        FROM light_groups
        GROUP BY lower(name)
        HAVING count(*) > 1
    ) THEN
        RAISE EXCEPTION
            'cannot apply group-management guardrails: duplicate group names exist after normalization';
    END IF;
END
$$;

DROP INDEX IF EXISTS light_groups_name_ci_unique;

ALTER TABLE light_groups
    ADD CONSTRAINT light_groups_name_valid
    CHECK (
        name = btrim(name)
        AND name <> ''
        AND octet_length(name) <= 64
    );

CREATE UNIQUE INDEX light_groups_name_ci_unique
    ON light_groups (lower(name));

-- PH3-B: first persisted Timer Mode configuration.
-- Timer Mode owns power only; color and brightness remain whatever the light
-- already has when it enters the mode.

ALTER TABLE lights
    DROP CONSTRAINT lights_mode_valid;

ALTER TABLE lights
    ADD CONSTRAINT lights_mode_valid
    CHECK (mode IN ('custom', 'test', 'timer'));

CREATE TABLE timer_schedules (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    target_type TEXT NOT NULL,
    light_id    TEXT REFERENCES lights(lifx_id) ON DELETE CASCADE,
    group_id    BIGINT REFERENCES light_groups(id) ON DELETE CASCADE,
    on_time     TIME WITHOUT TIME ZONE NOT NULL,
    off_time    TIME WITHOUT TIME ZONE NOT NULL,
    timezone    TEXT NOT NULL,
    enabled     BOOLEAN NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT timer_schedules_target_type_valid
        CHECK (target_type IN ('light', 'group')),
    CONSTRAINT timer_schedules_target_valid
        CHECK (
            (target_type = 'light' AND light_id IS NOT NULL AND group_id IS NULL)
            OR
            (target_type = 'group' AND group_id IS NOT NULL AND light_id IS NULL)
        ),
    CONSTRAINT timer_schedules_times_differ
        CHECK (on_time <> off_time),
    CONSTRAINT timer_schedules_timezone_not_blank
        CHECK (timezone = btrim(timezone) AND timezone <> '')
);

-- One Timer definition per explicit target. This does not attempt to express
-- cross-group overlap; the application validates enabled schedules so two
-- timers cannot intentionally own the same light.
CREATE UNIQUE INDEX timer_schedules_light_target_unique
    ON timer_schedules (light_id)
    WHERE light_id IS NOT NULL;

CREATE UNIQUE INDEX timer_schedules_group_target_unique
    ON timer_schedules (group_id)
    WHERE group_id IS NOT NULL;

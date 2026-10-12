-- Telling people about what agents build: a public product page, launch
-- briefs for the departments that announce releases, people who signed up
-- for news, and where visits, downloads and sign-ups came from.

ALTER TABLE release_products
    ADD COLUMN public               boolean NOT NULL DEFAULT false,
    ADD COLUMN tagline              text    NOT NULL DEFAULT '',
    ADD COLUMN audience             text    NOT NULL DEFAULT 'both'
                                    CHECK (audience IN ('developers', 'business', 'both')),
    ADD COLUMN pitch                text    NOT NULL DEFAULT '',
    ADD COLUMN website              text,
    ADD COLUMN announce_departments uuid[]  NOT NULL DEFAULT '{}',
    ADD COLUMN announce_patches     boolean NOT NULL DEFAULT false;

-- People who asked for news about a product, with the consent they gave.
CREATE TABLE launch_subscribers (
    id                uuid PRIMARY KEY,
    product_id        uuid        NOT NULL REFERENCES release_products (id) ON DELETE CASCADE,
    email             text        NOT NULL,
    segment           text        NOT NULL CHECK (segment IN ('developer', 'business')),
    company           text,
    source            text        NOT NULL,
    consent           text        NOT NULL,
    unsubscribe_token text        NOT NULL UNIQUE,
    subscribed_at     timestamptz NOT NULL DEFAULT now(),
    unsubscribed_at   timestamptz
);
CREATE UNIQUE INDEX launch_subscribers_email_idx ON launch_subscribers (product_id, lower(email));

-- Daily counts per source: no cookies, no IP addresses, nothing per person.
CREATE TABLE launch_events (
    product_id uuid   NOT NULL REFERENCES release_products (id) ON DELETE CASCADE,
    day        date   NOT NULL,
    source     text   NOT NULL,
    kind       text   NOT NULL CHECK (kind IN ('visit', 'download', 'signup')),
    count      bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (product_id, day, source, kind)
);

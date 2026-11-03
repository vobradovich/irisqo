

-- select 'retried' as name, count(*) as count from public.scheduled where is_retried
-- union all
-- select 'scheduled' as name, count(*) as count from public.scheduled where not is_retried
-- union all
-- select 'enqueued' as name, count(*) as count from public.enqueued where lock_at is null
-- union all
-- select 'processing' as name, count(*) as count from public.enqueued where lock_at is not null
-- union all 
-- select status::text as name, count(*) as count from public.processed group by status;

select (CASE is_retried WHEN TRUE THEN 'retried'::history_status ELSE 'scheduled'::history_status END) as name, count(*) as count from public.scheduled group by is_retried
union all
select (CASE lock_at is null WHEN TRUE THEN 'enqueued'::history_status ELSE 'assigned'::history_status END) as name, count(*) as count from public.enqueued group by lock_at is null
union all
select status::text::history_status as name, count(*) as count from public.processed group by status;

WITH a AS (
	INSERT INTO jobs( meta, headers, body)		
		SELECT
			'{"retry": "none", "timeout": 3000, "protocol": "none"}'::jsonb as meta,
			'{}'::jsonb as headers,
			null as body
		from generate_Series(1, 1000) s
	RETURNING id
)
INSERT INTO enqueued SELECT id FROM a RETURNING id

SELECT p.id, p.status, p.at - j.created_at, p.instance_id FROM processed p
inner join jobs j on j.id = p.id
order by p.id desc
limit 10
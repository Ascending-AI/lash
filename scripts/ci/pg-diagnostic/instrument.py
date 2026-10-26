"""Patch the pinned diagnostic extension with reset and sampling coverage metadata.

Collector and reader synchronize through the extension's existing collector lock.
Only the throwaway diagnostic image contains this patch.
"""
from pathlib import Path

p = Path('pg_wait_sampling.h')
s = p.read_text().replace('SHMRequest\trequest;', '''SHMRequest\trequest;
    uint64 reset_generation;
    uint64 sample_count;
    TimestampTz first_sample;
    TimestampTz last_sample;
    int64 max_gap_us;
    uint64 missed_intervals;''')
p.write_text(s)
p = Path('pg_wait_sampling.c')
s = p.read_text().replace('shm_toc_insert(toc, 0, pgws_collector_hdr);', '''MemSet(pgws_collector_hdr, 0, sizeof(CollectorShmqHeader));
        shm_toc_insert(toc, 0, pgws_collector_hdr);''')
s = s.replace('#include "postgres.h"', '#include "postgres.h"\n#include "utils/builtins.h"')
s += '''
PG_FUNCTION_INFO_V1(lash_wait_sampling_coverage);
Datum
lash_wait_sampling_coverage(PG_FUNCTION_ARGS)
{
    LOCKTAG tag;
    char *result;
    check_shmem();
    pgws_init_lock_tag(&tag, PGWS_COLLECTOR_LOCK);
    LockAcquire(&tag, ShareLock, false, false);
    result = psprintf("{\\"generation\\":%llu,\\"samples\\":%llu,\\"first_us\\":%lld,\\"last_us\\":%lld,\\"max_gap_us\\":%lld,\\"missed_intervals\\":%llu}",
        (unsigned long long)pgws_collector_hdr->reset_generation,
        (unsigned long long)pgws_collector_hdr->sample_count,
        (long long)pgws_collector_hdr->first_sample,
        (long long)pgws_collector_hdr->last_sample,
        (long long)pgws_collector_hdr->max_gap_us,
        (unsigned long long)pgws_collector_hdr->missed_intervals);
    LockRelease(&tag, ShareLock, false);
    PG_RETURN_TEXT_P(cstring_to_text(result));
}
'''
p.write_text(s)
p = Path('collector.c')
s = p.read_text().replace('pgws_collector_hdr->latch = &MyProc->procLatch;', '''pgws_collector_hdr->latch = &MyProc->procLatch;
    pgws_collector_hdr->reset_generation++;''')
s = s.replace('/* Iterate PGPROCs under shared lock */', '''/* Diagnostic sampling coverage. A reset and collector restart change generation. */
    if (write_profile)
    {
        LOCKTAG tag;
        pgws_init_lock_tag(&tag, PGWS_COLLECTOR_LOCK);
        LockAcquire(&tag, ExclusiveLock, false, false);
        if (!pgws_collector_hdr->first_sample) pgws_collector_hdr->first_sample = ts;
        if (pgws_collector_hdr->last_sample && ts - pgws_collector_hdr->last_sample > 20000)
            pgws_collector_hdr->missed_intervals++;
        if (pgws_collector_hdr->last_sample)
            pgws_collector_hdr->max_gap_us = Max(pgws_collector_hdr->max_gap_us,
                ts - pgws_collector_hdr->last_sample);
        pgws_collector_hdr->last_sample = ts;
        pgws_collector_hdr->sample_count++;
        LockRelease(&tag, ExclusiveLock, false);
    }
    /* Iterate PGPROCs under shared lock */''')
s = s.replace('/* Reset profile hash */', '''pgws_collector_hdr->reset_generation++;
                /* Reset profile hash */''')
p.write_text(s)

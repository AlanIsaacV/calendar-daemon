/* Parse an ICS file with libical, the same library the target calendar
   client uses, and report what it actually extracted. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <libical/ical.h>

static char *slurp(const char *path) {
    FILE *f = fopen(path, "rb");
    if (!f) { perror(path); exit(2); }
    fseek(f, 0, SEEK_END); long n = ftell(f); fseek(f, 0, SEEK_SET);
    char *buf = malloc(n + 1);
    if (fread(buf, 1, n, f) != (size_t)n) { fprintf(stderr, "short read\n"); exit(2); }
    buf[n] = 0; fclose(f); return buf;
}

int main(int argc, char **argv) {
    if (argc < 2) { fprintf(stderr, "usage: %s file.ics\n", argv[0]); return 2; }
    char *text = slurp(argv[1]);
    icalcomponent *root = icalcomponent_new_from_string(text);
    if (!root) { printf("PARSE FAILED: not iCalendar\n"); return 1; }

    int errors = icalcomponent_count_errors(root);
    printf("component: %s\n", icalcomponent_kind_to_string(icalcomponent_isa(root)));
    printf("unparseable lines: %d\n", errors);

    const char *calname = "(none)";
    for (icalproperty *p = icalcomponent_get_first_property(root, ICAL_X_PROPERTY);
         p; p = icalcomponent_get_next_property(root, ICAL_X_PROPERTY)) {
        const char *n = icalproperty_get_x_name(p);
        if (n && strcmp(n, "X-WR-CALNAME") == 0) calname = icalproperty_get_x(p);
    }
    printf("X-WR-CALNAME: %s\n", calname);

    int count = 0;
    for (icalcomponent *c = icalcomponent_get_first_component(root, ICAL_VEVENT_COMPONENT);
         c; c = icalcomponent_get_next_component(root, ICAL_VEVENT_COMPONENT)) {
        count++;
        const char *uid = icalcomponent_get_uid(c);
        const char *sum = icalcomponent_get_summary(c);
        struct icaltimetype st = icalcomponent_get_dtstart(c);
        icalproperty *urlp = icalcomponent_get_first_property(c, ICAL_URL_PROPERTY);
        const char *url = urlp ? icalproperty_get_url(urlp) : "(none)";
        icalproperty_status status = icalcomponent_get_status(c);
        printf("  event uid=%s allday=%d start=%s status=%s url=%s summary=%s\n",
               uid ? uid : "(none)",
               icaltime_is_date(st),
               icaltime_as_ical_string(st),
               icalproperty_status_to_string(status),
               url, sum ? sum : "(none)");
    }
    printf("events: %d\n", count);
    return errors > 0 ? 1 : 0;
}

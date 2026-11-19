#!/usr/bin/env python3
"""Build the synthetic content-sniffing test set: python3 -I scripts/lab_content_set.py <corpus-dir> <labels.toml>"""
from pathlib import Path
out = Path(sys.argv[1]); out.mkdir(parents=True, exist_ok=True)
F = {}
# --- answerable, bland names: content is the only signal
F["scan_0001.txt"] = ("Invoices", """INVOICE #4471
Bill to: Fernwood Landscaping Ltd
Date: 2026-03-02   Due: 2026-04-01
Description                    Qty   Unit     Amount
Lawn aeration                    2   120.00   240.00
Mulch delivery (cubic yard)      6    45.00   270.00
Subtotal 510.00  Tax 40.80  TOTAL DUE 550.80
Payment terms: net 30. Please remit to account ending 4410.
""")
F["scan_0002.txt"] = ("Invoices", """RECEIPT
Harbor Hardware  Store 22
2026-05-14 16:42
2x PVC elbow 3/4in      5.98
1x teflon tape          1.49
1x utility knife       12.99
Subtotal 20.46  Tax 1.64  Total 22.10
VISA ending 8823  APPROVED
Thank you for shopping with us.
""")
F["scan_0003.txt"] = ("Invoices", """STATEMENT OF ACCOUNT
Account holder: R. Albrecht      Statement period: Aug 1 - Aug 31
Previous balance          212.40
Payments received        -212.40
New charges (see detail)  187.65
Amount due by Sep 20:     187.65
Late payments incur a 1.5% monthly fee.
""")
F["note_a.txt"] = ("Correspondence", """Dear Ms. Okafor,

Thank you for meeting with me on Tuesday. I wanted to follow up on the points we
discussed about the lease renewal, and confirm that I will send the signed copy
by the end of next week.

Please let me know if you need anything else in the meantime.

Kind regards,
Daniel Pruitt
""")
F["note_b.txt"] = ("Correspondence", """From: Priya Natarajan
To: Sam
Subject: Re: Saturday

Hi Sam, Saturday works for us. We'll bring the folding chairs and Leo's
bike. Can you let the neighbours know it might be a little loud around noon?
Talk soon,
Priya
""")
F["note_c.txt"] = ("Correspondence", """Re: Claim No. HX-88213

To whom it may concern:

I am writing to dispute the decision dated 3 June regarding the water damage claim
on my property. I have enclosed the contractor's estimate and photographs, and I
ask that the claim be reviewed by a different adjuster.

Sincerely,
M. Lindqvist
""")
F["export_1.txt"] = ("Reports", """Q2 2026 OPERATIONS REPORT
Prepared by: Analytics team

Summary: Ticket volume fell 8% quarter over quarter. Median resolution time
improved from 31 to 24 hours. Backlog older than 14 days dropped to 41 tickets.
Recommendations: 1) keep weekend coverage, 2) retire the legacy queue.
""")
F["export_2.txt"] = ("Reports", """FINDINGS: Network latency review, branch offices

Method: 14 days of probe data from 6 sites.
Result: Site D shows p95 latency of 212 ms against a 60 ms target. Cause traced
to a saturated uplink during backups. All other sites within target.
Conclusion: Move backup window to 02:00-05:00.
""")
F["export_3.txt"] = ("Reports", """TEST RESULTS SUMMARY - build 2026.11.3
Total cases: 1,204   Passed: 1,189   Failed: 9   Skipped: 6
Failures concentrated in the export module (7 of 9). Regression introduced in
commit range a41c..d90e. Pass rate 98.8%, down from 99.6%.
""")
F["guide_1.txt"] = ("Manuals", """QUICK START GUIDE - Model T200 Air Purifier

1. Place the unit on a flat surface at least 20 cm from the wall.
2. Remove the protective film from the filter and reinsert it.
3. Plug in and press POWER. The indicator turns green.
4. Hold the MODE button 3 seconds to reset the filter timer.
Troubleshooting: if the light blinks red, check that the filter door is closed.
""")
F["guide_2.txt"] = ("Manuals", """Operating instructions
Cordless drill CD-18

Charging: insert the battery into the charger. Charge for 60 minutes. Do not charge
below 0 C. Selecting speed: slide the switch forward for high speed, back for high
torque. Always disconnect the battery before changing bits.
""")
F["guide_3.txt"] = ("Manuals", """CONFIGURATION REFERENCE
Option: retry_limit   Type: integer   Default: 3
  Number of attempts before a request is abandoned.
Option: timeout_ms    Type: integer   Default: 5000
  Maximum time to wait for a response, in milliseconds.
See the user guide, chapter 4, for worked examples of each option.
""")
# --- misleading names
F["invoice_final.txt"] = ("Correspondence", """Hi Marcus,

Just checking whether you've had a chance to look at the proposal. No rush, but the
vendor needs an answer by Friday so I can lock in the date.

Thanks,
Jo
""")
F["report.txt"] = ("?", """Lemon tart

200 g flour, 100 g butter, 1 egg yolk. Blind bake 15 minutes. Filling: 4 lemons,
150 g sugar, 3 eggs, 100 ml cream. Bake 25 minutes at 160 C until just set.
""")
F["manual.txt"] = ("?", """Roses are red, violets are blue,
the printer is jammed and so are you.
I fed it more paper, I fed it a prayer,
it ate them both up and left only air.
""")
# --- no category fits
F["list_1.txt"] = ("?", "milk\neggs\nspinach\nolive oil\nbread\nlemons\ncoffee beans\n")
F["list_2.txt"] = ("?", "Things to do this weekend:\n- fix the gate latch\n- call mum\n- return library books\n- clean the gutters\n")
F["data_1.csv"] = ("?", "t,temp_c,humidity\n0,21.4,40\n60,21.5,41\n120,21.7,41\n180,22.0,43\n240,22.1,44\n")
F["data_2.json"] = ("?", '{\n  "theme": "dark",\n  "font_size": 14,\n  "autosave": true,\n  "recent": ["a.txt", "b.txt"]\n}\n')
F["data_3.txt"] = ("?", "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor\nincididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam.\n")
F["data_4.txt"] = ("?", "Verse 1\nWalking home along the river\nStreetlights flicker one by one\nVerse 2\nEvery window holds a story\nBut the night has just begun\n")
F["data_5.txt"] = ("?", "ideas\n\nbird feeder with a camera\nmap of every bench in town\nwrite down grandpa's boat stories\n")
for name, (_, body) in F.items():
    (out / name).write_text(body)
with open(sys.argv[2], "w") as f:
    f.write("# Content-sniffing test: synthetic files with uninformative names, written by\n"
            "# hand before any run. Name tells nothing; only the content decides.\n"
            "# invoice_final/report/manual are deliberately misleading names.\n[labels]\n")
    for name, (lab, _) in F.items():
        f.write(f'"{name}" = "{lab}"\n')
print(len(F), "files;", sum(1 for v in F.values() if v[0]=="?"), "unanswerable")

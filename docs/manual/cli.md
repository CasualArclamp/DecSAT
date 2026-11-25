<!-- made by CasualArclamp -->
# DecSAT manual: Command line

This page gives examples of the `decsat` command line. [Back to the README](../../README.md).

```bash
decsat scan capture_8Msps.cs8     # find every carrier and identify each
decsat scan capture_8Msps.cs8 --decode   # …then run each one's decoder over the capture and report
decsat record --hackrf 1720:1780:14 --rate 20 --seconds 2 --out caps   # raw IQ from the HackRF: one slice or a sweep (receive only)
decsat scene                      # write the 8 MS/s test scene above
decsat cid                        # a DVB-S2 carrier with a DVB-CID under it
decsat test-signals tests         # both, into tests/ (see tests/README.md)
decsat mcast recording.ts --decode --record out   # the radio in a .ts (MPE)
decsat biss "123456 789ABC" scrambled.ts clear.ts   # a BISS-scrambled recording descrambled (says whether the key fits)
decsat payload fastlink.bin --tdm-out frames.bin  # a modem .bin through the payload stage
decsat decode --hackrf 1635.0 --decoder fastlink --offset 640e3 --bandwidth 200e3   --out dir --record-on-activity     # watch a FastLink live, record when a channel speaks
decsat decode qflex_333333Sps.cf32 --decoder fastlink --out dir --tdm-record   # a Q-Flex's call voice to a .wav
decsat decode capture_8Msps.cs8 --decoder ip --offset -1.2e6 --bbframes s2.bbf   # a DVB-S2 carrier, its good BBFRAMEs kept
decsat decode capture_8Msps.cs8 --decoder auto --offset 3.3e6 --bandwidth 60e3   # a Comtech CDM carrier: the type found, the model from its data
decsat decode capture_20Msps.cs8 --decoder tdma --offset -4.27e6 --bandwidth 5.3e6 --out dir   # a TDMA group: layout, slots, frame, terminals; bursts to a file
decsat decode capture_4Msps.cs8 --decoder is19 --offset -803.8e3 --bandwidth 125e3 --out dir   # an Intelsat 19 DAMA outbound: packets, call signalling; packets to a .csv
decsat decode capture_4Msps.cs8 --decoder beacon --offset 1.2e3 --bandwidth 250e3 --out dir   # a satellite beacon: carrier, telemetry frames (Record: .bin, .csv, .level.csv)
decsat payload trunk.bin --frame-bits 2223 --bit-rate 1024000 --voice-out voice.wav --radar-out tracks.csv --hdlc-out frames.txt
decsat decode --hackrf 1748 --rate 4000000 --decoder tpc --offset -915e3 --bandwidth 640e3 --wireshark   # a modem's IP live in Wireshark
decsat pcap recording.pcap --csv points.csv   # a recorded PCAP read back
decsat pcap recording.pcap --radio-csv radio.csv   # ED-137 radio signalling, packet by packet
decsat payload link.bin --frame-bits 2223 --bit-rate 537665 --pcap-out ip.pcap   # a data recording's IP, decoded again
decsat modcods                    # the DVB-S2 MODCOD table
```

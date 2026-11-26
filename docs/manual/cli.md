<!-- made by CasualArclamp -->
# DecSAT manual: Command line

This page gives examples of the `decsat` command line. [Back to the README](../../README.md).

```bash
decsat scan capture_8Msps.cs8     # find each carrier and identify it
decsat scan capture_8Msps.cs8 --decode   # then also run the decoder of each carrier on the capture and give a report
decsat scan capture.wav --decode --only bgan --out dir   # decode only the Inmarsat BGAN carriers, all at the same time, and write the IP of each one to a PCAP (without --out, the command keeps no files)
decsat record --hackrf 1720:1780:14 --rate 20 --seconds 2 --out caps   # record raw IQ from the HackRF: one slice or a sweep (receive only)
decsat scene                      # write the 8 MS/s test scene (the picture in the README shows it)
decsat cid                        # write a DVB-S2 carrier with a DVB-CID below it
decsat test-signals tests         # write the two test signals into tests/ (see tests/README.md)
decsat mcast recording.ts --decode --record out   # decode the radio stations in a .ts (MPE)
decsat biss "123456 789ABC" scrambled.ts clear.ts   # descramble a BISS recording (the command tells you if the key is correct)
decsat payload fastlink.bin --tdm-out frames.bin  # send a modem .bin through the payload stage
decsat decode --hackrf 1635.0 --decoder fastlink --offset 640e3 --bandwidth 200e3   --out dir --record-on-activity     # monitor a FastLink live and record when there is speech on a channel
decsat decode qflex_333333Sps.cf32 --decoder fastlink --out dir --tdm-record   # write the call voice of a Q-Flex to a .wav
decsat decode capture_8Msps.cs8 --decoder ip --offset -1.2e6 --bbframes s2.bbf   # decode a DVB-S2 carrier and keep its good BBFRAMEs
decsat decode capture_8Msps.cs8 --decoder auto --offset 3.3e6 --bandwidth 60e3   # a Comtech CDM carrier: DecSAT finds the type, and the model from the data
decsat decode capture_20Msps.cs8 --decoder tdma --offset -4.27e6 --bandwidth 5.3e6 --out dir   # a TDMA group: layout, slots, frame, terminals. The bursts go to a file
decsat decode capture_4Msps.cs8 --decoder is19 --offset -803.8e3 --bandwidth 125e3 --out dir   # an Intelsat 19 DAMA outbound: packets, call signalling. The packets go to a .csv
decsat decode capture_4Msps.cs8 --decoder beacon --offset 1.2e3 --bandwidth 250e3 --out dir   # a satellite beacon: carrier, telemetry frames (Record: .bin, .csv, .level.csv)
decsat payload trunk.bin --frame-bits 2223 --bit-rate 1024000 --voice-out voice.wav --radar-out tracks.csv --hdlc-out frames.txt
decsat decode --hackrf 1748 --rate 4000000 --decoder tpc --offset -915e3 --bandwidth 640e3 --wireshark   # show the IP of a modem live in Wireshark
decsat pcap recording.pcap --csv points.csv   # read a recorded PCAP again
decsat pcap recording.pcap --radio-csv radio.csv   # ED-137 radio signalling, for each packet
decsat payload link.bin --frame-bits 2223 --bit-rate 537665 --pcap-out ip.pcap   # decode the IP of a data recording again
decsat modcods                    # show the DVB-S2 MODCOD table
```

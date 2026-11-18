Public overview of Deepgram and of Pipecat and Daily (Deepgram companies), from https://deepgram.com/llms.txt, https://docs.pipecat.ai/llms.txt and https://docs.daily.co/llms.txt (fetched 2026-11-18). Public facts only; prices are list prices.

# Deepgram Voice AI Platform

> Deepgram is a foundational Voice AI platform for developers, offering unified APIs for Speech-to-Text (STT), Text-to-Speech (TTS), and autonomous Voice Agent orchestration. Available as a managed API (US, EU, and Australia endpoints), in-VPC, or self-hosted.

## Products
- Voice Agent API: A single WebSocket API unifying STT, LLM orchestration, and TTS with sub-300ms transcription latency. Flat-rate pricing from $4.50/hr, BYO LLM and BYO TTS options, function calling, barge-in, and mid-session reconfiguration.
- Speech-to-Text (STT):
    - **Flux**: Conversational speech recognition purpose-built for voice agents, with built-in end-of-turn detection, natural interruption handling, and word-level timestamps. Available in English (`flux-general-en`) and Multilingual (`flux-general-multi`, 10 languages with in-conversation language switching).
    - **Nova-3**: Flagship transcription model for batch and streaming, with 50+ languages, keyterm prompting, diarization, smart formatting, and noise robustness.
    - **Industry-tuned & custom models**: Domain models including Nova-3 Medical, plus custom models trained on customer data.
- Text-to-Speech (TTS):
    - **Flux TTS**: Streaming-first, conversation-native TTS on `/v2/speak` (WebSocket and batch REST). Generally available and the default Voice Agent voice (`flux-kit-en`). Barge-in reporting of exactly what the user heard, mid-session speed control, and 36 English voices across seven accents.
- Audio Intelligence: Summarization, sentiment analysis, topic detection, and intent recognition.

## Pricing
- **Pay As You Go**: $200 free credit, no credit card required, no minimums or expiration.
- **Growth**: Pre-paid annual credits from $4K/year, saving up to 20% with higher concurrency limits.
- **Enterprise**: Volume commitments, custom models, dedicated or self-hosted deployment, and SLAs — contact sales.

## Solutions
- Contact Centers: STT, TTS, and audio intelligence for CCaaS and contact center platforms, with integrations including Five9, Genesys, Twilio, Vonage, Amazon Connect, AudioCodes, and Kore.ai.
- Conversational AI: Real-time STT, TTS, and orchestration for voicebots, assistants, and enterprise voice agents.
- Medical & Healthcare: HIPAA-compliant Nova-3 Medical models trained on clinical terminology, with on-prem and VPC deployment options.
- Restaurants: Voice AI ordering (drive-thru, phone, kiosk), employee assist, call center automation, and reservations for enterprise restaurant brands and restaurant technology companies.
- Speech Analytics: Conversation analytics, intent detection, and QA at scale.
- Media Transcription: Captions, summaries, and searchable archives for podcasts, video, and broadcast.
- Partners: Technology and platform partners, including IBM — Deepgram provides the voice capabilities embedded in IBM watsonx Orchestrate's Agent Builder.

## Technical Benchmarks & Comparisons
Deepgram consistently outperforms legacy cloud and open-source models in latency and accuracy.

- OpenAI Whisper: Deepgram is 36% more accurate, up to 5x faster, and 1.4x cheaper.
- Amazon Transcribe: Deepgram is 23% more accurate, 10x faster, and 5.6x more affordable.
- Google Cloud STT: Deepgram is 53% more accurate, nearly 40x faster, and 5x more affordable.
- Microsoft Azure: Deepgram is 30% more accurate, more than 25x faster, and over 4x more affordable.
- AssemblyAI, Speechmatics, and ElevenLabs comparisons are also available.
- AI Voice Generator: Interactive tool for testing Deepgram voices in the browser.

## Developer Resources
- API Documentation: Official guides, API references, and SDK tutorials.
- API Playground: In-browser testing environment for STT, TTS, and Voice Agent endpoints.
- SDKs: Official JavaScript, Python, .NET, Go, and Java SDKs for Deepgram's speech recognition, text-to-speech, and language understanding APIs.
- UI Components: Prebuilt React components and browser agent widgets for voice interfaces.
- Deepgram CLI: Command-line access to STT, TTS, text intelligence, and account management, including a built-in MCP server and agentic developer tools.
- Changelog: Latest model releases, new language support, and API updates.
- Self-Hosted Deployments: Documentation for VPC and on-premise implementation.
- Rate Limits & Concurrency: Concurrency limits by plan and endpoint.
- Community and Support.

## Trust, Security & Data Residency
- SOC 2 Type 1 and Type 2 certified, HIPAA compliant (BAAs for Enterprise), GDPR ready, CCPA compliant, and PCI compliant.
- Regional endpoints for data residency: US (default), EU (`api.eu.deepgram.com`), and Australia (`api.au.deepgram.com`).
- Security Policy, Data Privacy Compliance, and Information Security & Privacy.

## Company & Learning
- Customer Stories and Powered by Deepgram.
- Startup Program: Credits and support for early-stage companies.
- Resource Hub and AI Glossary.
- About, Careers, and Newsroom.

# Pipecat

> Pipecat is an open source ecosystem for building voice and multimodal AI
> agents. Build with the Python framework, connect users with the client SDKs,
> structure conversations with Pipecat Flows, and deploy to production on
> Pipecat Cloud.

## About Pipecat

Pipecat is an open source framework and hosted platform for real-time voice
and multimodal AI agents. Its primary components:

- **Pipecat framework**: An open source Python framework that orchestrates AI
  services (STT, LLM, TTS, and more) into real-time pipelines. Start with the
  quickstart.
- **Pipecat client SDKs**: JavaScript, React, React Native, iOS, Android, and
  C++ SDKs that connect users to agents over WebRTC or WebSockets. See the
  client introduction.
- **Pipecat Flows**: A framework for structured conversations - define
  conversation paths as nodes with functions and actions. See the
  Flows introduction.
- **Pipecat Cloud**: Managed infrastructure for deploying and scaling agents,
  run by the Pipecat team. See the
  Cloud introduction.
- **Pipecat Enterprise**: The same Pipecat Cloud control plane, with agents
  running in a Kubernetes cluster you operate. See the
  Enterprise overview.
- **Pipecat CLI**: Scaffold projects, run evals, and deploy from the terminal.
  See the CLI overview.

Server API reference lives under `api-reference/server/`: services (STT, TTS,
LLM, transports, serializers), pipelines, frames, workers, and utilities.

# Daily
- Welcome to Daily Docs: Everything you need to build real-time video and audio into your app.
- Video Call Architecture (Quick Intro): Learn the basics of WebRTC, SFUs, and how Daily routes media between participants.
- Global Mesh Network (Mesh SFU): Daily's global infrastructure for delivering real-time video and audio to anyone, anywhere in the world.
- Daily call quality logging and metrics: Learn how to use Daily's call quality logs and metrics to understand and diagnose call quality issues.
- Using the Daily Dashboard: A tour of the Daily dashboard — manage rooms, inspect sessions, access API keys, and monitor usage.
- Daily Adaptive Bitrate: Automatically adjust video quality in real time based on network conditions, delivering the best visual experience your network can support.
- Debugging call issues: Diagnose connection failures, device problems, and audio or video quality issues in Daily calls, starting from the symptom.
- Debugging recordings and transcription: Diagnose recordings that never started, came out blank or incomplete, and transcription failures.
- Before contacting support: What to collect before filing a support ticket, so your issue gets answered in one round trip.
- Working with Daily meeting tokens: How to obtain, use, and validate Daily meeting tokens.
- Self-signing meeting tokens: Generate Daily meeting tokens locally using your API key, without making a round-trip to the Daily API.
- Controlling who joins a meeting: Use room privacy settings, meeting tokens, and knocking to restrict access to your Daily rooms.
- HIPAA Compliance for healthcare: Learn how to turn on HIPAA compliance with the Daily API.
- Setting up your CSP for daily-js: What CSP directives are required to work with `daily-js`.
- Networking Guide: Configuring corporate/enterprise networks to support Daily calls.
- Scaling Daily applications: Guides and best practices for building large-scale call experiences with Daily.
- Large real-time calls: Learn how to configure Daily rooms for WebRTC-powered calls of up to 1,000 active participants.
- Interactive live streaming and RTMP output: Learn how to build WebRTC-powered calls of up to 100,000 participants, or broadcast to external platforms via RTMP.
- Best practices to scale large experiences: Learn how to use pagination, track subscriptions, and simulcast layer control to optimize performance for large Daily calls.
- Recording calls with the Daily API: Learn about cloud, cloud-audio-only, local, and raw-tracks recording options for Daily calls.
- Storing Daily call recordings in a custom Amazon S3 bucket: Configure Daily to store call recordings directly in your own S3 bucket.
- Storing Daily call recordings in a custom OCI bucket: Configure Daily to store call recordings and transcripts directly in your own Oracle Cloud Infrastructure Object Storage bucket.
- Recording event JSON file: Format of the .event.json file written when a recording is started with dataOutputs: ["event-json"].
- Live streaming with Daily: Broadcast a real-time Daily video call to any RTMP streaming platform of your choice.
- Streaming HLS with Daily: Learn how to configure HTTP Live Streaming (HLS) with Daily, including S3 storage and multi-bitrate output.
- Multi-instance live streaming and recording: Run independent live streaming and recording instances in a single Daily room, each with its own layout.
- Custom layouts with VCS: Use Daily's Video Component System to build custom live streaming and cloud recording layouts with text overlays, image overlays, and more.
- SIP and PSTN dial-in/dial-out: Add phone and SIP connectivity to Daily rooms — dial-in with a PIN, pinless dial-in via webhook, SIP video, dial-out, DTMF, and call transfers.
- SIP dial-in and dial-out: Configure SIP audio and video dial-in and dial-out for Daily rooms, including multiple SIP endpoints and dynamic display names.
- PSTN dial-in with a PIN code: Let participants join a Daily room by dialing a phone number and entering a room-specific PIN code.
- SIP Interconnect and Pinless PSTN dial-in: Route incoming phone and SIP calls to Daily rooms via webhook, without requiring callers to enter a PIN code.
- SIP and PSTN call transfers: Transfer SIP and PSTN calls between Daily rooms or to external destinations using SIP Call Transfer or SIP REFER.
- SIP Trunk (inbound): Point your own SIP carrier at Daily and auto-provision a dial-in room for every inbound call — no per-room setup, no PIN.
- Twilio: Send inbound calls from Twilio Elastic SIP Trunking to your Daily SIP trunk.
- Telnyx: Send inbound calls from Telnyx SIP Trunking to your Daily SIP trunk.
- Vonage: Send inbound calls from Vonage to your Daily SIP trunk.
- SIP Clients: Register ordinary SIP clients (softphones like Linphone or baresip) to your domain over UDP or TLS, and place or receive calls without buying a number.
- Transcription: Daily supports both real-time transcription during calls and post-call transcription of recordings, both powered by Deepgram.
- Build audio-only experiences with Daily: Learn how to build voice-first and voice-only applications with Daily, including best practices and common feature patterns.

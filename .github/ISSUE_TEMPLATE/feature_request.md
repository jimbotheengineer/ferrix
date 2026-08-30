---
name: Feature request
description: Request a new feature
title: '[FEATURE] {{ title }}'
body:
  - type: textarea
    id: description
    attributes:
      label: 'Detailed description'
      description: 'Describe the feature you want'
      validations:
        required: true
  - type: textarea
    id: use_cases
    attributes:
      label: 'Use cases'
      description: 'Explain how this feature will be useful'
      validations:
        required: true
  - type: textarea
    id: alternatives
    attributes:
      label: 'Alternatives'
      description: 'What alternatives have you considered?'
---

## Feature request template

**Is your feature request related to a problem? Please describe.**

**Describe the solution you'd like**

**Describe alternatives you've considered**

**Additional context**

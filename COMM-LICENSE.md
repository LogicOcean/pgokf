# pgokf Commercial License Agreement

This Commercial License Agreement (the "Agreement") is between **LogicOcean**,
the licensor identified in full (legal name, entity form, and principal office
address) on the applicable Order Form (the "Licensor"), and the individual or
legal entity identified on that Order Form (the "Licensee").

pgokf is dual-licensed. The GNU Affero General Public License, version 3.0 only
([`LICENSE`](LICENSE), the "AGPL"), is available to everyone at no cost. This
Agreement is the alternative for a Licensee that cannot, or does not wish to,
comply with the AGPL's obligations. Nothing in this Agreement limits any right
granted by the AGPL, and the AGPL remains available regardless of this
Agreement.

This Agreement takes effect on the date both parties execute an Order Form that
references it (the "Effective Date"). **Absent an executed Order Form, no
rights are granted under this Agreement**, and any use of the Software is
governed solely by the AGPL.

## 1. Definitions

- **"Software"** means the pgokf source code and binaries as published by
  Licensor in the repository at `github.com/LogicOcean/pgokf` or any successor
  repository Licensor designates in writing, comprising the PostgreSQL
  extension, its supporting libraries (`okf-parser`, `okf-sync`), and the
  companion tools (`pgokf-ingest`, `pgokf-embed`, `pgokf-mcp`,
  `pgokf-pgconn`), including the versions covered under Section 2.
- **"Order Form"** means a written or electronic ordering document, executed
  by both parties, that references this Agreement and states at minimum the
  Licensee, Licensor's full legal identity, the fees, the License Term, the
  licensed scope (such as the number of Production Instances, products, or
  seats), the rights designated under Section 2, and the parties' notice
  addresses.
- **"Production Instance"** means a PostgreSQL server instance - physical,
  virtual, or containerized - in which the Software is installed and which
  serves live workloads of Licensee or its customers, including read replicas
  that serve queries. Instances used solely for development, testing,
  continuous integration, or unpromoted standby failover do not count against
  the licensed scope.
- **"Affiliate"** means an entity that controls, is controlled by, or is under
  common control with a party.
- **"Modifications"** means any derivative work of the Software made by or for
  Licensee.
- **"License Term"** means the period stated in the Order Form.
- **"Licensee Product"** means a product or service of Licensee that
  incorporates, embeds, or is operated using the Software or Modifications.

## 2. License grant

Subject to Licensee's payment of the fees and continued compliance with this
Agreement, Licensor grants Licensee a non-exclusive, non-transferable,
worldwide license, within the scope stated in the Order Form, to:

1. use, reproduce, and modify the Software and create Modifications;
2. if the Order Form designates embedding and distribution rights, embed and
   distribute the Software and Modifications, in source or object form, as
   part of a Licensee Product; and
3. if the Order Form designates hosting rights, operate the Software and
   Modifications to provide a Licensee Product to third parties over a network
   (including as a hosted or managed service),

in each case **without the copyleft and source-availability obligations of the
AGPL** (including sections 5, 6, and 13 of the AGPL). This Agreement covers
every version of the Software released by Licensor on or before the last day
of the License Term; a version is "released" when Licensor publishes it as a
tagged release. Provided all fees for the License Term have been paid in full,
the rights granted for those versions survive expiration or non-renewal of the
License Term (perpetual for licensed versions) and remain subject to Sections
3, 5, and 8 through 10; they do not survive termination by Licensor for
Licensee's uncured breach under Section 7. Versions released after the License
Term require a renewal.

The license runs to the Licensee named on the Order Form. Affiliates may
exercise it only if designated on the Order Form; their use counts against the
licensed scope, and Licensee is responsible for their compliance.

## 3. Restrictions

Licensee shall not:

1. distribute the Software or Modifications on a standalone basis, or as a
   product whose primary value is the Software itself (such as reselling,
   relicensing, or offering the Software as a catalog/extension product that
   competes with the Software), except as part of a Licensee Product with
   materially independent functionality. A Licensee Product has materially
   independent functionality if the Software is not the predominant source of
   the Licensee Product's value and the Licensee Product is not marketed as a
   substitute for the Software;
2. offer the Software's functionality to third parties as, or as a marketed
   feature of, a database, data-catalog, metadata, or infrastructure hosting
   service (including managed PostgreSQL), unless the Order Form expressly
   designates such rights;
3. remove or alter copyright, license, or attribution notices in the Software,
   except that in copies distributed under this Agreement Licensee may add a
   statement that the copy is distributed under a commercial license from
   Licensor and not under the AGPL;
4. use Licensor's names, marks, or logos except for factual attribution; or
5. sublicense the rights granted here except as embedded in a Licensee Product
   under Licensee's own end-user terms, which must (a) restrict use of the
   Software to use as part of the Licensee Product, (b) prohibit standalone
   redistribution or extraction of the Software, and (c) disclaim warranties
   and liability on Licensor's behalf to the extent permitted by law. Licensee
   is responsible for its end users' compliance with those terms.

## 4. Fees, taxes, and verification

Licensee shall pay the fees stated in the Order Form. Unless the Order Form
states otherwise, fees are invoiced on execution and payable net 30 in US
dollars, and overdue undisputed amounts bear interest at 1.5% per month or the
maximum lawful rate, whichever is less. Except as expressly stated in the
Order Form, fees are non-refundable. Licensee may dispute an invoice in good
faith by written notice before its due date, paying undisputed amounts when
due; amounts disputed in good faith are not overdue while the parties resolve
the dispute. Licensor may suspend the license for fees more than 30 days
overdue after written notice; suspension applies to new development and new
distribution of the Software, does not affect copies already distributed in
Licensee Products, and Licensor will lift it promptly on payment.

Fees are exclusive of taxes. Licensee is responsible for all sales, use,
value-added, goods-and-services, withholding, and similar taxes arising from
its orders, other than taxes on Licensor's net income. If law requires
Licensee to withhold from a payment, the amount payable increases so that
Licensor receives the full amount invoiced.

Licensee will keep accurate records of its use of the Software against the
licensed scope. No more than once per 12 months, on Licensor's written
request, Licensee will certify its actual use in writing and provide
reasonable supporting information. Use exceeding the licensed scope requires
payment of fees for the excess, at the Order Form rates or, absent a stated
rate, Licensor's then-current rates, from the date the excess use began;
payment of those fees is Licensor's exclusive remedy for excess use that
Licensee did not knowingly conceal, and knowingly concealed excess use is a
material breach. The obligations in this paragraph survive for 12 months after
expiration or termination.

## 5. Ownership and third-party components

The Software is licensed, not sold. Licensor and its contributors retain all
right, title, and interest in the Software, excluding the third-party
components described below. Licensee retains ownership of its Modifications,
subject to Licensor's underlying rights in the Software; nothing in this
Agreement obligates Licensee to disclose Modifications to Licensor or to
anyone else.

The Software incorporates third-party components licensed under their own
terms (for example MIT, Apache-2.0, BSD, and MPL-2.0 licensed libraries),
identified in the repository's dependency manifests and in any third-party
notices file shipped with a release. Those terms, not this Agreement, govern
those components; Licensee must retain their notices and, for MPL-2.0 licensed
files, comply with their source-availability terms on distribution. The AGPL
waiver in Section 2 applies only to obligations arising under the AGPL in code
in which Licensor holds sufficient rights.

## 6. Support

Licensor has no support, maintenance, or update obligation under this Agreement
unless stated in the Order Form.

## 7. Term and termination

This Agreement runs from the Effective Date for the License Term. Either party
may terminate for material breach on 30 days' written notice if the breach is
not cured within the notice period. Licensor may instead terminate immediately
on written notice for Licensee's breach of Section 3.1, 3.2, or 3.5. Either
party may terminate on written notice if the other becomes insolvent, makes a
general assignment for the benefit of creditors, or ceases business.

On termination by Licensor for Licensee's uncured breach, the licenses granted
here end, including the surviving rights described in Section 2, and Licensee
will cease use of the Software, destroy its copies of the Software and
Modifications, and certify destruction in writing on request. Termination does
not affect end users' rights to keep using copies of Licensee Products
distributed before termination, but Licensee may not distribute further copies
of, or updates containing, the Software; a hosted Licensee Product may
continue operating for up to 30 days after termination solely so that
Licensee's customers can transition. Expiration or any other termination
leaves the surviving rights in Section 2 in effect.

Sections 1, 3, 5, and 8 through 10, Licensee's accrued payment and
verification obligations under Section 4, and any license rights that survive
under Section 2 survive expiration or termination of this Agreement; any
surviving license remains subject to Sections 3, 5, and 8 through 10.

## 8. Warranty disclaimer

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING THE IMPLIED WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
PARTICULAR PURPOSE, TITLE, AND NON-INFRINGEMENT.

## 9. Indemnity and limitation of liability

Licensee will defend and indemnify Licensor and its personnel against
third-party claims, and the resulting costs and damages, arising from Licensee
Products, Modifications, Licensee's use of the Software in breach of this
Agreement, or Licensee's violation of law.

TO THE MAXIMUM EXTENT PERMITTED BY LAW, NEITHER PARTY IS LIABLE FOR ANY
INDIRECT, INCIDENTAL, SPECIAL, CONSEQUENTIAL, OR PUNITIVE DAMAGES, OR ANY LOSS
OF PROFITS, REVENUE, OR DATA, AND EACH PARTY'S AGGREGATE LIABILITY ARISING OUT
OF THIS AGREEMENT IS LIMITED TO THE GREATER OF (I) THE FEES PAID OR PAYABLE BY
LICENSEE IN THE 12 MONTHS PRECEDING THE EVENT GIVING RISE TO THE CLAIM AND
(II) US$500. NEITHER THE EXCLUSIONS NOR THE CAP IN THIS SECTION APPLIES TO (A)
LICENSEE'S BREACH OF SECTION 2 OR 3 OR ITS INFRINGEMENT OR MISAPPROPRIATION OF
LICENSOR'S INTELLECTUAL PROPERTY (OTHER THAN EXCESS USE REMEDIED UNDER SECTION
4), (B) FEES DUE UNDER AN ORDER FORM, (C) LICENSEE'S INDEMNITY OBLIGATIONS
UNDER THIS SECTION, OR (D) EITHER PARTY'S WILLFUL MISCONDUCT OR FRAUD. NOTHING
IN THIS AGREEMENT EXCLUDES LIABILITY THAT CANNOT BE EXCLUDED BY LAW, INCLUDING
FOR DEATH OR PERSONAL INJURY CAUSED BY NEGLIGENCE.

## 10. General

- **Entire agreement; precedence.** This Agreement with its Order Forms is the
  entire agreement between the parties regarding commercial licensing of the
  Software and supersedes prior discussions on that subject. If an Order Form
  conflicts with this Agreement, the Order Form controls for that order.
  Preprinted or linked terms on a Licensee purchase order, vendor portal, or
  similar document do not modify this Agreement, even if acknowledged or
  signed. This Agreement may be amended only in a writing signed by both
  parties. If a provision is unenforceable, the remainder stays in effect. A
  failure to enforce a provision is not a waiver of it.
- **Execution.** An Order Form may be executed in counterparts and by
  electronic signature, or by an exchange of emails in which each party's
  authorized representative expressly accepts the Order Form; each method
  constitutes execution.
- **Notices.** Notices must be in writing and sent to the addresses (including
  email addresses) stated on the Order Form; notice by email is effective on
  confirmed delivery. Either party may update its notice address by notice.
- **Confidentiality.** Each party will use the other's non-public business and
  technical information disclosed under this Agreement only for purposes of
  this Agreement, will not disclose it to third parties, and will protect it
  with reasonable care. This does not apply to information that is or becomes
  public without breach, was known before disclosure, is independently
  developed, or is rightfully received from a third party; a party may
  disclose what law compels after reasonable advance notice where lawful. The
  terms of each Order Form, including pricing, are confidential information of
  both parties.
- **Publicity.** Licensor may identify Licensee by name and logo as a customer
  in its marketing materials; Licensee may opt out at any time by written
  notice, and Licensor will honor the request within a reasonable period.
- **Feedback.** Feedback about the Software is voluntary and non-confidential;
  Licensor may use it for any purpose without obligation or attribution.
- **Assignment.** Neither party may assign this Agreement without the other's
  consent, except to a successor in a merger or sale of substantially all
  assets, on prompt written notice; Licensee may not assign to a direct
  competitor of Licensor without Licensor's consent.
- **Injunctive relief.** Licensee's breach of Section 2 or 3 would cause
  Licensor irreparable harm, and Licensor may seek injunctive relief in any
  court of competent jurisdiction in addition to its other remedies.
- **Compliance.** Each party will comply with applicable export control and
  sanctions laws, and Licensee represents that it is not located in an
  embargoed jurisdiction and is not on any denied-party list. The Software is
  "commercial computer software" under FAR 12.212 and DFARS 227.7202; US
  Government end users receive only the rights granted to all Licensees here.
- **Governing law.** This Agreement is governed by the law of the jurisdiction
  stated on the Order Form or, if the Order Form is silent, of the
  jurisdiction of Licensor's principal office as identified on the Order Form,
  in each case excluding its conflict-of-laws rules; the courts of that
  jurisdiction have exclusive jurisdiction, except as stated above for
  injunctive relief. The United Nations Convention on Contracts for the
  International Sale of Goods and UCITA do not apply.

## Obtaining a commercial license

Open an issue titled **"Commercial license inquiry"** at
[github.com/LogicOcean/pgokf/issues](https://github.com/LogicOcean/pgokf/issues)
with a way to reach you (do not post confidential details in the issue), and
Licensor will follow up privately with an Order Form. See
[`LICENSING.md`](LICENSING.md) for an overview of the dual-license model.
